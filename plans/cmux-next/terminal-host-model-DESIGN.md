# Terminal host model: one holder for every PTY, one parser in the daemon

Status: decided 2026-10-23 (PL-daemon, perf lead batch 1); slices in section 6. Ask (Lawrence):
"one process per terminal ... the original rationale was to make it more resilient in case a
terminal process crashed, but maybe there's another way we can handle it safely?" Related:
[pty-holder.md](pty-holder.md) (frozen tiny holder, D1-D6), [holder-protocol-v1.md](holder-protocol-v1.md)
(one PTY per holder; superseded for terminals by section 4 here),
[ghostty-integration-DESIGN.md](ghostty-integration-DESIGN.md) G2/G3/G5/G8 (agreed with
PL-ghostty 2026-10-23), [speed-of-sound-DESIGN.md](speed-of-sound-DESIGN.md) sec. 3.4 (floor: 2
processes, daemon threads O(cores)).

## 1. Today (code at feat-cmux-next 62c427ab5bd, fleet numbers)

- One `cmux-tui __terminal-host` process per terminal: the whole 324k-line `cmux-tui` binary.
  RSS 10.7 MB, footprint 2.1-2.5 MB per host (new-terminal-100x, aws-m4pro-6). Heavy session
  (150 workspaces): 392 processes, 172 hosts, 3,300 threads, daemon 797 threads.
- Threads per terminal: host 7 + 2 per client (signals, exit, clipboard, parser, pty reader,
  child wait, client writer/reader); daemon 4 (`surface-N-frames`, `surface-N-host`,
  `surface-N-host-frames`, `terminal-host-reaper`) + 4-5 per attached app view.
- Three full VT parses per output byte (host `HostShared.term`, daemon mirror, app Ghostty) and
  three 10 MB scrollbacks. The host is "authoritative"; the daemon mirror seeds a replacement host.
- Resilience today: a host crash is survived because the daemon holds a custody dup of the PTY
  master (SCM_RIGHTS on every connection, `unix/pty_custody.rs`) and rehosts from its mirror
  (`surface/rehost.rs`); a daemon crash is survived because hosts keep running and are adopted.
  The shell only dies when every master fd closes. That, not the process per terminal, is what
  keeps shells alive.
- Create cost on the request path: host bootstrap + Launch round trip + record barrier, 4.3-4.9 ms
  per terminal (spans, aws-m4pro-6), more without a spare host.

## 2. Options

Numbers for 172 terminals (the heavy session). "Blast radius" = what a crash of one process
costs the user.

| Option | Processes cmux adds | Threads (terminal part) | Parses per byte | Holder/host crash | Daemon crash | Create on request path |
|---|---|---|---|---|---|---|
| A. Today: host per terminal | 172 | ~2,200 (9 host + 4 daemon each) | 3 | that terminal redraws (rehost from mirror) | all redraw from hosts | 4.5 ms (process + record barrier) |
| B. One process: daemon owns PTYs and parses | 0 | O(cores) | 2 | n/a | **every shell dies** (last master fd closes) | ~0.7 ms (posix_spawn) |
| C. Sharded hosts, 16 terminals each | 11 | ~11 x 9 + 4 x 172 | 3 | 16 terminals redraw | all redraw | ~4 ms |
| D. **One holder (no VT) + daemon parses + custody dups** | 1 | ~2 holder + O(cores) daemon | 2 | **no shell dies, no redraw**: the daemon still holds every master and its VT state; a new holder takes the fds back | no shell dies; all redraw from the holder's byte rings + last journal snapshot | ~1 ms (holder posix_spawn + one round trip) |
| E. Holder + VT worker pool (16 per worker) | 12 | ~12 x 3 + O(cores) | 2 | as D | control plane survives; 16 terminals redraw per worker crash | ~1 ms + a worker hop for every screen read, snapshot and OSC event |

Memory per terminal: A 2.3 MB host + 1.5 MB daemon mirror; D/E: the host's 2.3 MB goes (the
holder keeps a byte ring per terminal, resident only as filled, 1 MiB cap in memory, spill to its
log); the daemon mirror stays (it is the one parser). Heavy session: about 400 MB less, 171
processes and ~2,000 threads fewer.

## 3. Decision: D

- The thing that keeps shells alive is that some process holds the PTY master, not that each
  terminal has its own process. D keeps two holders of every master (the holder and the daemon),
  so a crash of either loses no shell; only both at once can, as today.
- The holder parses nothing, so the code that can crash on hostile output (the VT parser, Kitty
  graphics, OSC handling) runs in exactly one place outside the app: the daemon. pty-holder.md D2
  (Lawrence: "the daemon parses; the holder keeps only raw output") already decided this.
- E's isolation buys little: the app parses the same bytes with the same Ghostty, so an output
  that crashes the parser also crashes the app's surface; and E adds a hop to every screen read
  agents make. If telemetry ever shows daemon VT crashes, the daemon's parse can move into a
  worker later without touching the holder (the holder wire is the same).
- Poison pill: a daemon that crashed while parsing would replay the same bytes on restart and
  crash again. No per-chunk disk write guards it; the holder counts, per terminal, daemon links
  that ended without DETACH while it streamed that terminal. A terminal whose stream ended that
  way twice within 10 s is restored from its prologue (the newest snapshot) without the ring
  tail (the daemon gets a GAP and cancels), so one bad stream never loops the daemon. Built
  (review 2): `Terminal::strikes` in `cmux-holder/src/holder.rs`; a daemon sends DETACH for
  each terminal at shutdown, so a clean exit is no strike.

## 4. Holder (crate `cmux-holder`, std + libc only)

- One process per install (bundle id + tag), started by the daemon on first use; `setsid`,
  ignores SIGHUP/SIGINT/SIGTERM except from launchd (pty-holder.md 5.6). Built (review 2): a
  SIGTERM whose sender is pid 1 (logout, shutdown) ends the holder; a holder with no terminal
  and no daemon link exits after 30 s and removes its socket, so an idle holder of an old
  build does not serve new daemons for ever. Still open: identity record and lease as
  holder-protocol-v1 sections 7-8, and handing a busy old-build holder's terminals to a new
  build (an ADOPT handoff).
- Children (review 2): every descriptor the holder inherited is close-on-exec from its first
  instruction, and each shell gets back the soft `RLIMIT_NOFILE` cmux started with
  (`CMUX_TUI_HOST_NOFILE_SOFT`, as a spawned terminal host). A frontend's shell starts through
  login(1) on macOS as on the hosted path; CLOSE and its SIGKILL go to the child's group and
  the PTY's foreground group (no process-table scan, pty-holder.md 4): an interactive shell in
  its own group under login(1) is the foreground group, so it ends with its tab and hangs up
  its own jobs.
- One kqueue: the daemon link socket, every PTY master (EVFILT_READ), every child
  (EVFILT_PROC NOTE_EXIT, then `waitpid` for the status: the holder is the shells' parent). No
  thread per terminal; no timers.
- Per terminal: id (u128), master fd, child pid/pgid, a byte ring (1 MiB) with absolute offsets,
  exit status once reaped.
- The ring is shared memory (wire v4, holder item 1, 2026-10-23): a `memfd` on Linux, an unlinked
  `shm_open` object on macOS, header `magic "CMHR" | layout | capacity | base | end` then the data
  (`cmux-holder/src/ring.rs`). The holder `read`s the PTY straight into it and publishes `end` after
  each read; SPAWNED, LISTED and ADOPT pass the segment as a second descriptor (`FLAG_RING`) and the
  daemon keeps a custody dup next to the master's. A holder that dies loses at most the one read
  whose end it had not published (before: up to 256 KiB of unsent ring plus 1 MiB of queued frames,
  and the next holder numbered the PTY's next byte as if they had arrived: a silent splice). The
  daemon never maps the segment; it hands it to the next holder, which continues the same ring.
  Wire v5 (review 2 of holder items 1-3): ring layout 2 (a published `oldest`) passes only between
  peers of v5 or later, so a v4 holder never gets a segment it cannot read; a v5 holder answers
  every ADOPT with ADOPTED (`oldest u64, end u64`, plus the segment it really writes: the one
  handed over, or a fresh one when that was bad or stale), and the daemon keeps exactly that
  segment as custody. ATTACH carries the offset the daemon parsed, so the credit after an adoption
  or a resume counts from there and the ring never runs more than the credit window plus
  `UNSENT_HIGH_WATER` (768 KiB) past what the daemon parsed.
- Wire: multiplexed holder-protocol frames, `magic "CMHX" | kind u16 | flags u16 | chan u32 |
  len u32 | payload` (chan = terminal slot). Kinds: HELLO/WELCOME (token, version), SPAWN (argv,
  env, cwd, size; reply SPAWNED with pid and the master fd by SCM_RIGHTS: the daemon's custody
  dup), ADOPT (fds by SCM_RIGHTS + ids + offsets: a new holder takes over after a holder crash),
  ATTACH from offset, DATA (offset, bytes), GAP, ACK (trim), WRITE, RESIZE, SIGNAL, CLOSE, EXIT
  (status, end offset), LIST (every terminal: id, pid, offsets, state), PING/PONG, ERROR.
- Input: the daemon writes keystrokes to its own master dup directly (one hop less than today);
  WRITE exists for a daemon that lost its dup. The holder leaves the master non-blocking (the flag
  is shared by every dup), so the daemon's writer waits for POLLOUT on WouldBlock.
- Link speed and fairness (review 2): each end asks for 512 KiB socket buffers (macOS gives
  8 KiB); a reader keeps a cursor and compacts once per `recvmsg`; a DATA frame is built
  straight from the ring. Each pump starts at the next terminal in turn and gives every
  terminal with output an equal share (at least 16 KiB), and frames up to 4 KiB go ahead of
  queued bulk output unless their own terminal has bulk queued (one terminal's frames never
  pass each other), so an echo does not wait behind other terminals' floods.
- CLOSE of a terminal that already ended but whose EXIT an attached daemon has not got yet
  only marks it closing: the holder forgets it after it sent the rest of the output and EXIT.
- Flow control (wire v2, review 1): ACK carries the offset the daemon parsed. The holder sends an
  attached terminal at most 512 KiB past its last ACK and stops reading a master that holds
  256 KiB unsent, so a flood blocks in the PTY as on a direct PTY: no byte is dropped while a
  daemon is attached and the daemon's queue per terminal stays bounded. A DATA offset past what
  the daemon has, or a GAP, cancels the parser's current sequence (CAN) instead of splicing.
- Handshake: a connection becomes the link only after a valid HELLO; a probe or a bad token never
  drops the attached daemon. The daemon it replaces gets ERROR SUPERSEDED and does not reconnect.

## 5. Daemon side

- One holder link thread (kqueue/poll on the holder socket) feeds the existing output path; the
  per-terminal `surface-N-host`, `-host-frames` and `terminal-host-reaper` threads go away, and
  the frame producer becomes one shared thread (pending work: a set of dirty surfaces).
- The daemon mirror is the authority (agreed with PL-ghostty): it answers DA/DSR, serves snapshots,
  and forwards raw bytes to attached views; G5 binary attach frames (`attach-binary-output-v1`,
  header `\xFFCO kind:u8 len:u32 offset:u64`, kinds 1 output, 2 grid marker, 3 JSON event, 4/5
  snapshot) replace base64 JSON; a resize sends a kind-2 marker, a snapshot only on a digest
  mismatch (G2).
- Restore after a daemon crash: LIST, then per terminal its last journal VT snapshot at offset N
  and ATTACH from N; GAP resets and replays from the oldest kept byte, then a same-size RESIZE
  nudges full-screen programs to redraw. Built (review 2, wire v3) without the journal: the
  daemon's reader hands the holder a SNAPSHOT (a VT replay of its parsed screen, at most
  128 KiB, at the exact offset the parser reached) once a burst of 64 KiB went idle or every
  2 MiB of a flood; the holder keeps the newest as the terminal's prologue, and a restarted
  daemon's ATTACH gets it first and the ring from its base. A prologue older than the ring is
  still sent (modes survive) and the daemon cancels before the tail; a re-attach from a ring
  that dropped bytes starts with CAN. Still open: the same-size RESIZE nudge.
- A surface's close (Cmd-W) runs under its runtime lock, so it never connects, starts a holder
  or re-adopts (review 2): without an open link the CLOSE waits for the next link and the
  daemon hangs up the shell's groups itself.
- Holder crash: the daemon still has every master and its parsed state; it starts a new holder
  and ADOPTs every fd. Output written while no holder reads waits in the kernel's PTY buffer (the
  shell blocks, nothing is lost). Exit statuses of shells whose parent died read
  `exit-unobserved`, as today's rehost.
- The replay race at a holder crash (holder item 1, built 2026-10-23): the daemon replays from its
  last fully parsed and confirmed offset, never from the holder's send offset. Its sink keeps each
  queued byte's holder offset (runs, so a seed, a prologue or a CAN takes none), a chunk the reader
  took stays pending until the reader confirms it right after `vt_write`, under the terminal lock
  (`HolderSink::confirm_parsed`), and ACKs carry that confirmed offset. The hand-off reads the
  screen replay and the pending bytes at one instant under the same lock (no drain wait; before:
  a 1 s wait for the queue, then a prologue that claimed `received` while the reader could still
  hold a taken chunk). ADOPT (v4) then carries `base` = confirmed, `resume` = received, the ring
  segment, and a prologue of the replay (the OSC 9;4 progress before the parser's pending sequence,
  which goes last) plus the seed the parser has not finished; the new holder sends this daemon the
  ring from `resume` (the bytes the dead holder read but never sent) and a restarted daemon the
  prologue and the ring from `base`. A terminal from an older holder (no ring) gets a segment the
  daemon makes (`ring::create_segment`) and a prologue of the replay plus every unparsed byte at
  `received`; a v3 holder gets that prologue without a segment. Residual: a control string larger
  than the parser's pending-sequence budget (a multi-MiB Kitty upload) open at the crash is not
  replayed (`vt_replay_resumes_stream` false). Test: `holder_terminals.rs`
  `a_holder_killed_holding_unparsed_output_or_mid_sequence_loses_no_byte` (red on the base: the
  screen stopped after one 512 KiB credit window and the rest of a 10,000-line flood never came).

## 6. Slices

1. `cmux-holder` crate and binary, its wire, and the crash-injection test (a test daemon spawns
   16 shells through a holder, SIGKILLs the holder, ADOPTs the fds into a new holder: every shell
   alive, input and output work, the test's VT screen equals the one before; then drops the daemon
   side and re-attaches from the ring: screen restored). Run on the fleet.
2. Daemon: new terminals on the holder behind `CMUX_TUI_TERMINAL_HOST_MODEL=holder` (default
   off), one link thread, custody dups, restore; the existing host path stays for old terminals.
   Built (2026-10-23): `holder_runtime/` (service, sink, terminal), `surface/holder_spawn.rs`,
   `mux/holder_adoption.rs`; `cmux-tui __holder` and `cmux-tui-host __holder` run the holder.
   A holder terminal runs on the daemon's local runtime (`PtyRuntime::Local` with its holder),
   so the daemon's parser is the only one outside the app. Test: `tests/holder_terminals.rs`
   (no host record; daemon SIGKILL and restart restores every screen from the ring; holder
   SIGKILL keeps every shell and screen; a second restart re-attaches from the new holder).
   Slice 3 (2026-10-23): the reopen seed reaches the parser first; a holder that cannot restart
   ends its terminals lost (respawn path); on ADOPT the daemon hands the new holder a VT replay
   of each screen (its prologue, at most 768 KiB), which the holder sends first to a restarted
   daemon, so a restart after a holder crash restores the whole screen. Fleet A/B (aws-m4pro-7,
   send to reply p50/p95, hosts -> holder): new-tab 31.9/56.9 -> 13.2/55.6 ms, new-workspace
   33.6/52.3 -> 14.1/15.1 ms, unsized split 25.7/47.8 -> 13.5/14.9 ms; 0 host processes.
   Still open: reader, wait and frame threads per terminal (the shared frame producer lands
   from the daemon-speed workstream); default on (slice 4).
   Holder items 1-3 (2026-10-23): wire v4 shared rings and the confirmed-offset hand-off (section
   5); the standalone frozen `cmux-holder` binary, bundled beside `bin/cmux` and started from its
   content-addressed copy, adopting older holders by wire version (pty-holder.md 2); first-text
   timing in holder mode (`debug_spans::output_marks`, new-terminal-100x `host_model=`).
3. G5 binary attach frames and the in-band resize marker (with PL-ghostty's decode).
4. Default on; old `__terminal-host` processes keep their client until none is left (pty-holder 2.2).

## 7. Durability of replies (decided 2026-10-23, perf lead)

No daemon reply waits for the disk. Registry commits (and the conversation store's and chief
registry's) run `synchronous=NORMAL`: a commit is in the WAL, in the kernel's page cache, when it
returns, and a crash, kill or update of the daemon keeps it. One syncer thread per database runs
the WAL checkpoints off every lock (SQLite syncs the WAL before each), and flushes the drive cache
(F_FULLFSYNC) once at close (`workspace_registry/wal_durability.rs`). These are SQLite's own
`synchronous=NORMAL` WAL rules, with the checkpoint moved off the request. Trade-off: a power loss
or kernel panic can take back the tail of replies since the last checkpoint that the kernel had not
yet written back (a tail, never out of order). The same loss ends every shell, host and holder, so
the terminals those replies described are gone anyway; a workspace rename or layout change made in
that tail is lost. A sync per reply was measured and rejected: the next request's WAL writes
waited for it (same fleet mini, new browser tab 4.2 ms p50 without, 22-25 ms with F_FULLFSYNC,
about 11 ms with F_BARRIERFSYNC). Before: each commit F_FULLFSYNCed inside the registry lock (about
4.5 ms on a fleet M4 Pro, 20 ms on a busy Apple SSD), 1-3 per create or close reply, and every
request that needed the lock waited behind it.

## 8. The app draws screens, not bytes (task p01-process-241, spike 2026-10-27)

Goal (chrome-parity p01-process.md, Rule of 2): the app parses no terminal output. A VT worker
parses each byte once and publishes the screen; the app draws what it is handed, as Chrome's
browser composites what renderers rasterize (`components/viz/common/quads/compositor_frame.h`:
a frame and its damage). Until this lands the app's Ghostty parses every untrusted byte, so a VT
or Kitty bug can end cmux.

Built (`cmux-tui/crates/cmux-vt-worker/src/screen_page.rs`, the first file of the task 133 crate):

- The screen page: one terminal's viewport (cell text, width, authored colors, attributes,
  underline style), the cursor, the default colors and up to 64 visible Kitty placements by
  reference (`image_id`, `placement_id`, generation, viewport cell, grid size; pixels stay on the
  trusted `ghostty_surface_apply_kitty_replay` path). Shared `u64` words, one writer, many readers,
  no lock: Chromium's one-writer seqlock (`device/base/synchronization/one_writer_seqlock.h`).
- Damage: each row keeps the sequence of the publish that wrote it, the header the sequence of the
  last full invalidation (first publish, resize). A reader asks for the damage since the publish
  it drew last, so a reader that misses publishes misses no row.
- `PageWriter::publish_damage` reads only the damaged rows out of the render state
  (`RenderState::walk_damage`, new in ghostty-vt). `build_frame` copies every cell of the screen:
  about 300 us for a one-byte echo at 200x50, which every per-frame consumer of `build_frame`
  pays today.
- The page is untrusted (Chrome's bad_message rule): `PageReader::read` checks every count
  against its own geometry and every field's encoding, and returns `BadPage(<reason>)` for the
  owner to end the worker. A read page has no C0, DEL or C1 control in any cell.
- `encode_replay`: the canonical VT that draws an update on a Ghostty surface through the existing
  `ghostty_surface_process_output` (cursor moves, SGR, EL, text, DECSCUSR, DECTCEM). It never
  scrolls the surface and sets the column again after each non-ASCII cell, so a different width
  table cannot shift the row.

Tests (`cargo test -p cmux-vt-worker`, Linux x86_64, 8 pass): five corpora (typing, colored
`ls`, a 3,000-line flood, a full-screen TUI with curly underlines and cursor styles, CJK, emoji,
ZWJ and combining marks, full last rows) drawn from the page give the worker's screen cell by
cell and the same cursor, from `build_frame` and from `publish_damage`; a reader that misses
publishes gets every damaged row; resize damages every row; an echo publishes one row and a
replay under 64 bytes; each of 10 named corruptions gives its `BadPage` reason; 2,000 randomly
corrupted pages never panic, and no replay of an accepted one sets a title or scrolls.

Spike numbers (`cargo run --release -p cmux-vt-worker --example screen_page_spike`; 200x50
terminal in a 256x64 page of 323 KiB; one publish per chunk, a chunk stands for one frame;
Linux x86_64 VM at load 200-600, so absolute times are high and the ratios are the result; not
fleet numbers):

| corpus | output bytes | frames | replay bytes | app parse, bytes mode | app parse, screen mode | worker update + publish_damage p50/p99 | app read p50/p99 | encode p50/p99 | replay parse p50/p99 per frame |
|---|---|---|---|---|---|---|---|---|---|
| typing, 1 B echo per frame | 174 | 133 | 22,077 | 0.03 ms | 0.33 ms | 9.9/27 us | 3.9/6.0 us | 1.1/1.8 us | 2.0/6.1 us |
| `ls -l` colored, 4 KiB frames | 370,000 | 91 | 407,692 | 9.5 ms | 2.6 ms | 457/499 us | 151/195 us | 37/56 us | 27/49 us |
| flood, 100k lines, 64 KiB frames | 6,500,000 | 100 | 403,365 | 24.7 ms | 1.8 ms | 439/471 us | 151/175 us | 32/52 us | 17/35 us |
| full-screen TUI, 500 redraws | 4,291,154 | 501 | 4,504,054 | 17.2 ms | 20.2 ms | 506/586 us | 219/274 us | 63/84 us | 38/58 us |
| CJK and emoji, 4 KiB frames | 154,000 | 38 | 314,803 | 5.1 ms | 5.7 ms | 442/484 us | 125/147 us | 24/40 us | 146/204 us |

What the numbers say:

- The app's parse cost becomes O(frames x damaged rows), not O(bytes): a flood is 14x less app
  parse work (24.7 to 1.8 ms), `ls` 3.6x. A program that repaints the whole screen every frame
  costs about the same (1.2x more), and CJK the same (each non-ASCII cell adds a column move).
- An echo: the app's part is read 3.9 + encode 1.1 + parse 2.0 = 7 us, against 0.2 us to parse
  the byte. The worker's part is 10 us with `publish_damage`; with `build_frame` it was 366 us,
  which alone would break the 1 ms echo budget under STRESS. The hop from the worker to the app
  (a page-advance event) is not measured here; it is the same hop the daemon ring takes today.
- With `?7l` around the whole replay the app parsed it 2.5x slower (Ghostty's print fast path
  needs autowrap), so the replay keeps autowrap on and starts every row with an absolute move.
- Full damage costs the worker about 440-500 us per 200x50 frame, most of it `fill_cell` over
  the FFI (about 40 ns a cell). It runs once per frame, not per byte, and on the worker's
  thread, never the app's.

Not built into the app in this task. What it needs, none of it on perf/chrome-parity at
611363dd393:

1. The worker process (task 133) and its page mapping to the app: today no process publishes a
   page, and the app's ring reader (task 131, `TerminalOutputLane.processRing`) is not on the
   branch either. The app maps the page read-only next to the D3 ring; the stream gets a
   `page-advance(seq)` event in place of `RING_ADVANCE` for screen-mode surfaces.
2. History. The page is the viewport. A screen-mode surface keeps no scrollback of its own (the
   replay never scrolls it), so scrolling, search, selection across history and copy need the
   history from the worker: the existing GHOSTSNP history pages (`restoreSnapshot` history
   phase), which the app then reads from the worker's own encoder, not from program output.
3. Terminal state the app reads from its surface today: modes for key and mouse encoding (task
   209 puts them in the page), title and cwd (worker events, validated as task 141), prompt
   marks, hyperlinks (OSC 8 is not in the page), bell and notifications.
4. Strict acceptance. "VT parses per byte == 1" holds for program output: the app parses only the
   replay, which carries only escapes the encoder writes. A Ghostty API that takes cells (a
   `ghostty_surface_apply_screen` in GhosttyKit, a fork change) would remove that second parse;
   the replay is the way to ship without a fork change, and the numbers above say it is cheap
   enough for every corpus but full-screen CJK.

Plan, in order: (a) task 133 lands the worker with this page; (b) screen mode in the app behind
the default-off feature `terminalScreenPages` (schemas/features/features.d), with
`TerminalOutputLane.processScreen(update)` beside `processRing` on the same lane and the same
backlog, reading the page with this crate's reader rule ported to Swift and the history from (2);
(c) fleet A/B: `bench-typing.py` STRESS in both modes and the VT fuzz crash corpus with one
shard ending while the app keeps running; (d) the GhosttyKit cell API only if (c) shows the
replay parse past 1 ms p99 on a fleet Mac.
