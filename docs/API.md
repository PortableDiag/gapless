# Gapless control API

A local HTTP API that does everything the window does: load music, play it, seek
it, change the modes, rate a track, change the playback settings. Built so an
agent or another program can drive the player without a person at the keyboard.

**This document ships inside the binary.** `GET /api/docs` serves this exact
file from the running build, so it can never describe a different version than
the one answering you.

---

## Turning it on

Off by default. **Settings (the gear button) → Remote control API**: the switch,
the port, and the key.

The key is generated on first run and kept in `~/.config/gapless/api-key`, mode
0600. Read it without opening a window:

```sh
gapless --api-key
```

That works whether or not the player is running, and whether or not the API is
switched on — which matters, because asking the API for its own key is not a
plan that goes anywhere.

Default address: **`http://127.0.0.1:8421/api`**. The port is configurable in
the same popover. It binds to loopback only, never a wildcard — this is a local
control socket, not a service to put on a network.

---

## Authentication

Every endpoint needs the key. There is no unauthenticated route, not even a
health check.

```sh
KEY=$(gapless --api-key)
BASE=http://127.0.0.1:8421

curl -s -H "Authorization: Bearer $KEY" "$BASE/api/status"
curl -s -H "X-API-Key: $KEY"            "$BASE/api/status"   # same thing
```

A **401** means the key is wrong or missing, not that the header is. Both header
forms work.

Regenerating the key in the popover invalidates the old one immediately and
restarts the listener, so anything holding the old key stops working — which is
the point of the button.

---

## Rules that will bite you

- **Check the status code, not curl's exit code.** `curl` exits 0 on a 400. Use
  `-f`, or read the `error` key. Every error comes back as
  `{"ok": false, "error": "..."}` with a message that says what to send instead.
- **A typo in a mode is refused, not obeyed.** `{"mode":"favourites"}` is a 400.
  The API deliberately does not fall back to a default, because a script with a
  spelling mistake would otherwise silently change the mode to something else.
- **Values can go in the JSON body or the query string**, and the body wins when
  both carry the same key. `curl -X POST "$BASE/api/volume?volume=0.5"` needs no
  body at all.
- **`index` is a position in the current queue, not a stable id.** Opening a
  different folder renumbers everything. If you are holding onto a track across
  an `open`, hold its `path` — `/api/rating` accepts one.
- **Ratings are stored by path**, in `~/.config/gapless/ratings.json`, and are
  never written into your audio files.
- Trailing slashes don't matter: `/api/play` and `/api/play/` are one route.
- A known route with the wrong verb answers **405** and says so, rather than
  404. That distinction saves a round of guessing about whether the endpoint
  exists.

---

## Endpoints

`GET /api` lists them all with a one-line description each, so a caller that has
the key needs nothing else to get started.

| | |
|---|---|
| `GET /api` | the endpoint index |
| `GET /api/docs` | **this document**, as Markdown; `?section=` narrows it |
| `GET /api/status` | everything about the current state |
| `GET /api/queue` | the loaded tracks |
| `POST /api/play` | `{index?, position_secs?}` |
| `POST /api/pause` | |
| `POST /api/playpause` | |
| `POST /api/stop` | |
| `POST /api/next` | |
| `POST /api/previous` | |
| `POST /api/seek` | `{position_secs}` or `{offset_secs}` |
| `POST /api/volume` | `{volume}` |
| `POST /api/repeat` | `{mode: off\|all\|one}` |
| `POST /api/shuffle` | `{mode: off\|on\|favorites}` |
| `POST /api/rating` | `{index\|path, stars}` |
| `POST /api/open` | `{path}` — a folder or a playlist |
| `GET /api/settings` | playback settings |
| `POST /api/settings` | `{trim_silence?, crossfade_secs?, inner_silence_secs?}` |
| `POST /api/listen` | `{port}` — move the API to another port, for a handover |
| `GET /api/autostart` | whether Gapless starts at login |
| `POST /api/autostart` | `{enabled}` |
| `POST /api/quit` | close the player |

---

## Reading the state

```sh
curl -s -H "Authorization: Bearer $KEY" "$BASE/api/status"
```

```json
{
  "ok": true,
  "version": "0.3.0",
  "playing": true,
  "loaded": true,
  "position_secs": 63.6,
  "volume": 1.0,
  "repeat": "all",
  "shuffle": "favorites",
  "trim_silence": true,
  "crossfade_secs": 5.0,
  "inner_silence_secs": 0.0,
  "audio_sink": "pulsesink",
  "queue_length": 45,
  "source": "/home/me/Music/album",
  "track": {
    "index": 6,
    "title": "Pumping and Humping",
    "artist": "Austrian Death Machine",
    "album": "Triple Brutal",
    "year": 2019,
    "genre": "Thrash Metal",
    "disc": 1,
    "track_no": 7,
    "duration_secs": 169.4,
    "format": "MP3 · 44.1 kHz · 320 kbps · Stereo",
    "rating": 4,
    "path": "/home/me/Music/album/07 Pumping and Humping.mp3"
  }
}
```

`audio_sink` is **what audio is really going to** — the element `autoaudiosink`
actually chose. Check it. `playing: true` with `position_secs` climbing is *not*
proof anything is audible: a sink that failed to open the device leaves the
pipeline playing and the position advancing while nothing reaches the speakers,
and there is no other way to tell that apart from working.

`track` is **the track the now-playing panel is showing** — which is not always
the one making noise. On a fresh launch it is the track cued from the last
session, waiting for a play that hasn't happened yet; `loaded` and `playing`
tell those apart. `track` is `null` when nothing is cued at all.

`GET /api/queue` returns `{"ok": true, "count": N, "tracks": [...]}`, each track
in the same shape as `status.track`, in playing order.

---

## Playing something

```sh
# Load a folder, or a playlist file — the same endpoint takes both.
curl -s -X POST -H "Authorization: Bearer $KEY" \
     -H 'Content-Type: application/json' \
     -d '{"path":"/home/me/Music/Austrian Death Machine"}' \
     "$BASE/api/open"
# -> {"ok":true,"count":45}

# Start at the top.
curl -s -X POST -H "Authorization: Bearer $KEY" "$BASE/api/play"

# Or at a particular track, part-way in.
curl -s -X POST -H "Authorization: Bearer $KEY" \
     -H 'Content-Type: application/json' \
     -d '{"index":6,"position_secs":30}' "$BASE/api/play"
```

`POST /api/play` with no arguments does what the play button does: resume if
something is paused, start the track cued from the last session if one is, else
start at the beginning of the queue. `409` means nothing is loaded — `open`
something first.

Seeking takes an absolute position or a relative offset:

```sh
curl -s -X POST -H "Authorization: Bearer $KEY" -d '{"position_secs":90}' "$BASE/api/seek"
curl -s -X POST -H "Authorization: Bearer $KEY" -d '{"offset_secs":-10}'  "$BASE/api/seek"
```

Every transport endpoint answers with the full `status` object, so you do not
need a second call to see what happened.

---

## Ratings

Stars are **0–5**, where 0 clears the rating.

```sh
# By queue position.
curl -s -X POST -H "Authorization: Bearer $KEY" \
     -d '{"index":0,"stars":5}' "$BASE/api/rating"

# By path — survives a rescan that renumbers the queue.
curl -s -X POST -H "Authorization: Bearer $KEY" \
     -d '{"path":"/home/me/Music/album/01 Track.mp3","stars":3}' "$BASE/api/rating"

# Neither: rates whatever is playing.
curl -s -X POST -H "Authorization: Bearer $KEY" -d '{"stars":4}' "$BASE/api/rating"
```

The answer carries the updated track. `6` stars is a 400, not a clamp to 5. A
path that isn't in the current queue is a 404 — load its folder first.

Ratings live in `~/.config/gapless/ratings.json`, keyed by absolute path, and
are written the moment they change. Nothing is written into the audio files.

---

## Shuffle, including the favorites mode

```sh
curl -s -X POST -H "Authorization: Bearer $KEY" -d '{"mode":"favorites"}' "$BASE/api/shuffle"
```

Three modes:

| `off` | the queue plays in order |
| --- | --- |
| `on` | an ordinary shuffle |
| `favorites` | still a shuffle — every track plays once per pass — but the order is drawn with higher-rated tracks weighted towards the front |

`favorites` is **an ordering, not a filter.** Weights double per star (1★ = 1,
2★ = 2, 3★ = 4, 4★ = 8, 5★ = 16) and unrated sits at 2, level with 2★. A 1-star
track still comes up, just rarely.

Measured over 4,000 passes of a 12-track queue, where a plain shuffle puts every
track at a mean slot of 5.50: **5-star 1.51, unrated 5.96, 1-star 7.65.**
`cargo test -- --nocapture` prints that line.

Note that **MPRIS cannot express this.** Its `Shuffle` property is a bool, so
both on-modes publish as `true` there. Setting MPRIS `Shuffle = true` on a player
already in favorites mode leaves it in favorites rather than demoting it.

---

## Playback settings

The three things behind the gear button.

```sh
curl -s -H "Authorization: Bearer $KEY" "$BASE/api/settings"
# -> {"ok":true,"trim_silence":true,"crossfade_secs":0.0,"inner_silence_secs":0.0}

curl -s -X POST -H "Authorization: Bearer $KEY" \
     -d '{"crossfade_secs":3.0,"trim_silence":false}' "$BASE/api/settings"
```

- `trim_silence` — skip silence recorded at track edges. This is the setting that
  makes a non-gapless rip sound gapless.
- `crossfade_secs` — 0 to 10. **0 is gapless**, and is not a separate code path:
  the crossfade collapses to exact concatenation.
- `inner_silence_secs` — 0 to 10; caps silence left *inside* a track. 0 leaves
  tracks alone.

Out of range is a 400 and changes nothing. Send at least one field or you get a
400 saying so, rather than a successful call that did nothing.

---

## Replacing a running player without a gap

Stopping the player and starting it again leaves the room silent for as long as
the new copy takes to come up — and for as long as an installer takes, if you are
updating at the same time. **`scripts/handover.sh` does it with no hole in the
music**, measured at a **34 ms** overlap on this machine.

```sh
./scripts/handover.sh                        # e.g. after an update
./scripts/handover.sh /path/to/other/gapless
```

It works because of two flags:

| | |
|---|---|
| `--new-instance` | run a second copy instead of handing the request to the one already running. `GApplication` is single-instance by default, which is right for a desktop launcher and makes a handover impossible. |
| `--api-port N` | listen somewhere else for this run. The outgoing copy still owns the configured port. **Not saved** — a handover's scratch port must not become the configured one. |

The sequence, and the reasoning:

1. Start the replacement on a scratch port. It loads the library and cues the
   track **while the old copy is still playing**; this is the slow part and it
   costs nothing.
2. Match volume and modes, and pre-roll it **paused** at the right position, so
   the audio pipeline is built before the swap.
3. Read the outgoing position at the last possible moment, then play the
   replacement and pause the old one back to back. They **overlap** for a few
   tens of milliseconds rather than leaving a hole — that is the right way
   round: a listener notices silence, not a brief doubling.
4. Quit the old copy; the replacement takes the configured port with
   `POST /api/listen`.
5. **Check a real audio stream exists**, not just that the API says `playing`.

The second copy does not get the MPRIS name — the first still owns it — and says
so on stderr rather than failing.

---

## Errors

| | |
|---|---|
| **400** | the request is wrong — the message says what to send |
| **401** | missing or incorrect key |
| **404** | no such endpoint, or no such track/path/file |
| **405** | that route exists, but not with that verb |
| **409** | the player isn't in a state where that makes sense (nothing loaded, nothing playing) |
| **413** | body over 64 KB |
| **500** | the audio engine refused; the message is GStreamer's |
| **503** | the player is shutting down |

---

## Notes for agents

- **Start with `GET /api/status`.** It answers "is anything loaded", "is it
  playing", "what are the modes" in one call, and every transport endpoint
  returns the same object, so a poll loop is rarely needed.
- **`GET /api/docs?section=Ratings`** narrows this document to one section.
  Matching is case-insensitive and by substring. A name that matches nothing
  answers 404 with the list of sections that exist.
- **Commands run on the UI thread, one at a time**, in the same place a button
  click runs. Two callers cannot interleave halfway through a queue change, and
  an API call and a click cannot either.
- **Changes are visible in the window immediately** and are saved to
  `~/.config/gapless/state.json` the same way a click would save them — the API
  and the UI drive one player, not two copies of the state.
- **Don't poll `/api/status` in a tight loop for the position.** It is reported
  from the pipeline; a 1-second interval is plenty, and the player checkpoints
  its own position to disk every 5 seconds regardless.
- **`POST /api/quit` saves the session first**, so the resume point survives. It
  replies before it exits.

---

## Verifying it

```sh
DISPLAY=:0 ./scripts/verify-api.sh
```

Launches the real application on a private D-Bus session, a private config
directory and a high port, and exercises the whole API over a real socket: that
it refuses an absent key and a wrong one, that an unauthorised write changes
nothing, that routing and status codes are what this document says, that ratings
reach the sidecar file, that modes reach `state.json` after a **SIGKILL**, and
that nothing answers on the port when the switch is off. Volume is set to zero
before anything plays, so the run is silent.
