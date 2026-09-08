# Working on this repository

pcmflux is the audio-capture and encode library behind
[selkies](https://github.com/selkies-project/selkies), and is developed together with it and with
[pixelflux](https://github.com/selkies-project/pixelflux) (screen capture and video encode). A change in one often
belongs in another; coordinate across all three.

Use web search, web fetch, and other available tools as necessary. Make sure that the comments or documentation are
not too verbose (do not add comments more fit for a PR summary than a comment). Do not leave arbitrary numbers (such
as issue or task numbers) in the code or documentation. Do not use inline comments. Do not use comments or
documentation that describe arbitrary code changes of previous states compared to the current code that do not need
explanation. The code commenting should reflect the current state of the codebase and be used to convey information
to an LLM bot or developer.

Empirical testing is possible for everything here, including implementation, auditing, validation and verification,
and every change is validated before it is reported. `cargo test --lib` is the floor, and
`cargo test --release bench_emit_assembly -- --ignored --nocapture` prints the assembly measurement to quote rather
than assert. End to end, a change is a wheel (`pip wheel . --no-deps`) installed into a selkies sandbox as the
Agentic Development section of that repository's `docs/development.md` describes, driven by its audio suites over
both transports with the installed Firefox and Chrome and Playwright/Selenium/Puppeteer/Cypress WebKit in place of
Safari. Ask before building an environment on a machine that was not set up for one (Miniforge serves a host with a
closed package manager) and take the operator's directives on how it is constructed and constrained. Say which checks
could not run where the hardware for them was not available.

Note that parity between X11 and Wayland, as well as between WebSockets and WebRTC, or between the default dashboard
and the wish dashboard, is considered a key focus (things that were not wired up correctly on either side, and similar
discrepancies, are subject to fixes or deduplication). I prefer deduplicating code that performs similar purposes
across different modes over keeping duplicate code for no reason and more fragility. Refactor through deduplication if
you are confident there will be no regressions (or able to validate regressions). Screen coroutine usage in both
Python and JavaScript, as well as thread usage in all languages, so that everything is performant and does not lead to
hanging or lagging. Performance preservation or improvements such as zero-copy and latency-reducing measures are
always important. Note that compatibility should be ensured for Python 3.9 to 3.14 or even higher.
A defect that predates the change you are making is still in scope: finding it does not make it someone else's,
and "pre-existing" is not a reason to leave it. Fix it, or say precisely what is broken, what you ruled out, and
what you would do next. The same applies to a failure you cannot reproduce yet -- narrow it until it is either
fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

`LICENSES.md` inventories the crates and the linked libpulse (LGPL-2.1-or-later) and libopus (BSD-3-Clause);
`pcmflux/deny.toml` keeps the crate graph permissive (the `Licenses` workflow runs it). A new crate that links
native code gets a row there.

Update this file when certain details change.
