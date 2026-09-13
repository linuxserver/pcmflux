/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Ogg Opus fan-out on a Unix socket: every consumer that connects receives the stream's
//! `OpusHead` and `OpusTags` pages, then one page per encoded packet, so the socket reads
//! as a standard Ogg Opus stream (`ffmpeg -i unix:<path>`, the pixelflux recorder).
//!
//! The tap never perturbs the capture: the capture thread only hands each page to a
//! bounded per-consumer channel drained by that consumer's writer thread, and a consumer
//! that falls behind is dropped rather than waited on.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Per-consumer backlog: two seconds of 20 ms packets before a consumer is dropped.
const CLIENT_QUEUE_CAP: usize = 100;
/// The Ogg page checksum's polynomial, applied to the whole page with the field zeroed.
const CRC_POLY: u32 = 0x04c1_1db7;

/// What `OpusHead` tells a decoder; `mapping` is `(streams, coupled, table)` for channel
/// mapping family 1, absent for family 0 (mono and stereo).
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub mapping: Option<(u8, u8, Vec<u8>)>,
}

struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

/// One Ogg Opus stream served to every consumer of a Unix socket.
pub struct OggSink {
    path: String,
    clients: Arc<Mutex<Vec<Client>>>,
    shutdown: Arc<AtomicBool>,
    serial: u32,
    seq: u32,
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ CRC_POLY } else { crc << 1 };
        }
    }
    crc
}

/// One page carrying one packet: the lacing table splits it into 255-byte segments, a
/// final short segment (empty when the length is a multiple of 255) closing it.
fn page(serial: u32, seq: u32, granule: u64, header_type: u8, packet: &[u8]) -> Vec<u8> {
    let mut lacing: Vec<u8> = vec![255; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut out = Vec::with_capacity(27 + lacing.len() + packet.len());
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    out.extend_from_slice(packet);
    let crc = crc32(&out).to_le_bytes();
    out[22..26].copy_from_slice(&crc);
    out
}

/// The identification and comment headers as the stream's first two pages.
fn header_pages(serial: u32, head: &OpusHead) -> Vec<u8> {
    let mut id = b"OpusHead".to_vec();
    id.push(1);
    id.push(head.channels);
    id.extend_from_slice(&head.pre_skip.to_le_bytes());
    id.extend_from_slice(&head.input_sample_rate.to_le_bytes());
    id.extend_from_slice(&0i16.to_le_bytes());
    match &head.mapping {
        None => id.push(0),
        Some((streams, coupled, table)) => {
            id.push(1);
            id.push(*streams);
            id.push(*coupled);
            id.extend_from_slice(table);
        }
    }
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(b"pcmflux".len() as u32).to_le_bytes());
    tags.extend_from_slice(b"pcmflux");
    tags.extend_from_slice(&0u32.to_le_bytes());
    let mut out = page(serial, 0, 0, 0x02, &id);
    out.extend_from_slice(&page(serial, 1, 0, 0, &tags));
    out
}

impl OggSink {
    /// Bind the socket at `path`, or None when no path is configured or the bind fails:
    /// the sink is optional and never takes the capture down.
    pub fn try_bind(path: &str, head: &OpusHead) -> Option<Self> {
        if path.is_empty() {
            return None;
        }
        let _ = fs::remove_file(path);
        let listener = match UnixListener::bind(path).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[pcmflux] ogg sink bind failed on {path}: {e}");
                return None;
            }
        };
        let serial = std::process::id() ^ (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0));
        let headers = Arc::new(header_pages(serial, head));
        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (clients_acc, shutdown_acc, path_log) = (clients.clone(), shutdown.clone(), path.to_string());
        thread::spawn(move || {
            while !shutdown_acc.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE_CAP);
                        let stop = Arc::new(AtomicBool::new(false));
                        let stop_writer = stop.clone();
                        let _ = tx.try_send(headers.clone());
                        thread::spawn(move || {
                            let mut stream = stream;
                            for bytes in rx.iter() {
                                if stop_writer.load(Ordering::Relaxed) || stream.write_all(&bytes).is_err() {
                                    break;
                                }
                            }
                        });
                        clients_acc.lock().unwrap().push(Client { tx, stop });
                        eprintln!("[pcmflux] ogg sink consumer connected on {path_log}");
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL_INTERVAL),
                    Err(e) => {
                        eprintln!("[pcmflux] ogg sink accept error: {e}");
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });
        Some(Self { path: path.to_string(), clients, shutdown, serial, seq: 2 })
    }

    /// Send one packet as a page; `granule` is the 48 kHz sample count at its end.
    pub fn write_packet(&mut self, packet: &[u8], granule: u64) {
        let bytes = Arc::new(page(self.serial, self.seq, granule, 0, packet));
        self.seq = self.seq.wrapping_add(1);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| match c.tx.try_send(bytes.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                c.stop.store(true, Ordering::Relaxed);
                eprintln!("[pcmflux] ogg sink dropping a consumer that stopped reading");
                false
            }
        });
    }
}

impl Drop for OggSink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for c in self.clients.lock().unwrap().drain(..) {
            c.stop.store(true, Ordering::Relaxed);
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    /// A consumer that connects reads the two header pages and then every packet page,
    /// each with a checksum the Ogg polynomial reproduces and the granule it was given.
    #[test]
    fn stream_pages_are_well_formed() {
        let path = format!("/tmp/pcmflux-ogg-test-{}.sock", std::process::id());
        let head = OpusHead { channels: 2, pre_skip: 312, input_sample_rate: 48000, mapping: None };
        let mut sink = OggSink::try_bind(&path, &head).expect("bind");
        let mut consumer = UnixStream::connect(&path).expect("connect");
        consumer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        thread::sleep(Duration::from_millis(150));
        sink.write_packet(&[0xfc, 1, 2, 3], 960);
        sink.write_packet(&vec![0xfc; 255], 1920);
        thread::sleep(Duration::from_millis(150));
        let mut buf = vec![0u8; 4096];
        let mut got = Vec::new();
        while let Ok(n) = consumer.read(&mut buf) {
            if n == 0 { break; }
            got.extend_from_slice(&buf[..n]);
            if got.len() >= 27 * 4 + 19 + 27 + 4 + 259 { break; }
        }
        let mut pages = Vec::new();
        let mut at = 0;
        while at + 27 <= got.len() {
            assert_eq!(&got[at..at + 4], b"OggS");
            let segments = got[at + 26] as usize;
            let body: usize = got[at + 27..at + 27 + segments].iter().map(|&l| l as usize).sum();
            let end = at + 27 + segments + body;
            let mut copy = got[at..end].to_vec();
            let stored = u32::from_le_bytes(copy[22..26].try_into().unwrap());
            copy[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(crc32(&copy), stored, "page {} checksum", pages.len());
            let granule = u64::from_le_bytes(copy[6..14].try_into().unwrap());
            pages.push((granule, copy[27 + segments..].to_vec()));
            at = end;
        }
        assert_eq!(pages.len(), 4);
        assert!(pages[0].1.starts_with(b"OpusHead"));
        assert_eq!(pages[0].1[9], 2);
        assert!(pages[1].1.starts_with(b"OpusTags"));
        assert_eq!(pages[2], (960, vec![0xfc, 1, 2, 3]));
        assert_eq!(pages[3].0, 1920);
        assert_eq!(pages[3].1.len(), 255);
        drop(sink);
        assert!(!std::path::Path::new(&path).exists());
    }
}
