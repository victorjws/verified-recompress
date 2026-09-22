# verified-recompress

Re-encode Filen cloud files to more efficient formats without losing data.

The tool walks a [Filen](https://filen.io) drive through rclone, re-encodes what it can into a
denser format, proves the new file reproduces the old one, and only then replaces it. JPEG becomes
JPEG XL, WAV becomes FLAC, and video is left alone unless you explicitly opt in. Nothing is
deleted on a guess.

## How it keeps your data

Every conversion is assigned a *recipe*, and every recipe has a stated fidelity. Nothing is
converted without one.

| Recipe | Source | Output | Fidelity |
| --- | --- | --- | --- |
| `jxl-from-jpeg` | JPEG | JXL | **byte-exact.** `djxl -J` rebuilds the original file bit for bit |
| `jxl-from-raster` | PNG, GIF, BMP, TIFF | JXL | content-exact: identical pixels, new container |
| `flac` | WAV, AIFF | FLAC | content-exact, and byte-exact in practice via `--keep-foreign-metadata` |
| `flac-recompress` | FLAC, ALAC in `.m4a` | FLAC | content-exact: same samples at `-8` |
| `ts-remux` | MPEG-TS | MP4 | content-exact: stream copy, recovers container overhead only |
| `ffv1` | rawvideo, huffyuv, utvideo, … | MKV | content-exact: truly lossless, around 93% smaller |
| `av1` | other lossy video | source container | **lossy.** Gated, opt-in, irreversible |

The safety rules that follow from this:

- **Originals are deleted only after the replacement is proven.** The upload is confirmed by its
  size and by a server-side blake3 hash (`operations/hashsumfile`), so verification needs no
  re-download.
- **Deleted means "in the Filen trash".** The trash still counts against your quota. The default
  `trash_policy` is `keep`, so your usage will not drop until you run `cleanup --execute`. Until
  then every original is recoverable from the Filen web app.
- **AV1 is the only lossy path, and it is off by default.** It requires `--allow-video`, and each
  encode must clear a measured VMAF gate: mean >= 97.0 and 1st percentile >= 95.0, scored with
  `vmaf_v0.6.1` (or `vmaf_4k_v0.6.1` above 1080p). `ab-av1 crf-search` finds the CRF that meets
  the floor rather than assuming one. An encode that misses the gate is discarded, not uploaded.
- **A whole-drive write must be asked for.** `run --execute` without `--path` refuses to start
  unless you also pass `--all`.
- **Ctrl-C stops new work but lets in-flight jobs finish or roll back**, so the remote is never
  left mid-replacement.

## Requirements

`verified-recompress` shells out to established encoders rather than reimplementing them. All of
these must be on `PATH`:

| Tool | Needed for | Notes |
| --- | --- | --- |
| `rclone` | all remote access | **1.73 or newer**, which is when the Filen backend reached Tier 1 |
| `cjxl` / `djxl` | image tier | `djxl` must support `-J` / `--reconstruct_jpeg`, or JPEGs cannot be rebuilt |
| `ffmpeg` | video tier | must be built with `libsvtav1` **and** the `libvmaf` filter |
| `ffprobe` | classifying video | ships with ffmpeg |
| `flac` | audio tier | needs `--keep-foreign-metadata` |
| `ab-av1` | AV1 CRF search | finds the lowest CRF that still clears the VMAF gate |
| `exiftool` | metadata comparison in `bench` | |
| `taskset` | CPU budgeting | Linux only; elsewhere `preflight` warns and CPU limits are not enforced |

Run `preflight` before anything else. It checks for the *capabilities* it uses, not merely for the
binaries: an ffmpeg built without `libsvtav1` passes a `which` check and then fails halfway
through a run, so preflight probes the encoder and filter directly and fails loudly up front.

## Install

Set up the rclone remote first:

```sh
rclone config          # create a remote of type `filen`, named `filen`
rclone lsd filen:      # confirm it works
```

Then build:

```sh
cargo build --release
./target/release/verified-recompress preflight
```

## Quick start

The commands are meant to be run in this order. Each one is safe to repeat.

```sh
# 1. Confirm the environment. Fails loudly if anything is missing.
verified-recompress preflight

# 2. Build the inventory. Downloads nothing, writes nothing to the remote.
verified-recompress scan

# 3. See what a run would do and what it would save. Read-only.
verified-recompress plan

# 4. Dry run over a narrow slice. Without --execute nothing is written.
verified-recompress run --path /Photos/2019 --limit 10

# 5. The same thing for real.
verified-recompress run --path /Photos/2019 --limit 10 --execute

# 6. Prove the conversions. Byte-exact ones are rebuilt and hash-compared.
verified-recompress verify

# 7. See logical savings, quota change, and what is still held in the trash.
verified-recompress report

# 8. Empty the trash. This is the step that actually shrinks the drive,
#    and it is irreversible.
verified-recompress cleanup --execute
```

Steps 6 and 8 are the point of the tool. Skipping `verify` and going straight to `cleanup`
throws away the safety net.

For video, insert a measurement pass before opening the tier up:

```sh
verified-recompress bench --sample 5                      # compare presets on your own files
verified-recompress run --path /Videos --allow-video --preset 4 --execute
```

## Commands

Each command's **results go to stdout** with no log decoration, so they pipe:

```sh
verified-recompress plan | grep saving
```

Everything else — progress, the spinner, advice, and every external command and rclone API call
that was actually issued — is a diagnostic and goes to the log on **stderr**:

```sh
verified-recompress scan 2> scan.log     # the log
verified-recompress scan &> run.log      # log and results together
```

`-v` turns on the command log, which is the first thing to look at when a step is slower than it
should be. `-vv` adds the polling traffic on top. The rc password is masked in both.

Global flags work with every subcommand:

| Flag | Meaning |
| --- | --- |
| `--config FILE` | config file path (default `~/.config/verified-recompress/config.toml`) |
| `--remote REMOTE` | rclone remote, e.g. `filen:` (default `filen:`) |
| `--path PATH` | scope to a remote path. Repeatable. Replaces `paths` from the config file |
| `--exclude GLOB` | exclude a glob. Repeatable. Added to `exclude` from the config file |
| `-v`, `-vv` | raise log verbosity. `RUST_LOG` overrides it |

### `preflight`

Checks that every external tool exists and supports the flags used. Exits non-zero on any error
finding; warnings (such as a missing `taskset` off Linux) do not fail it.

### `scan`

Lists the remote and records every file in the ledger. Downloads nothing, modifies nothing. Run it
again whenever the drive changes.

One recursive request covers a whole scope, so a large drive can spend minutes inside a single
call. A counter runs while it does, which is how you tell a slow listing from a stuck one:

```
listing /
  ⠹ listed 86,068 entries · 00:00:15
```

Filen answers that request in bulk and then decrypts every name locally, so the counter sits at
zero for the fetch and climbs during the decrypt. When stderr is not a terminal the same updates
go out as a log line every 15 seconds instead.

```
Inventoried 12,043 file(s) across 2 scope(s).
  pending 11,890  done 0  skipped 153  failed 0  total 412.7 GB
```

### `plan`

Assigns a recipe to every pending file and reports projected savings, grouped by recipe and by
skip reason. Read-only.

The projection uses conservative per-recipe ratios, not measurements, and says so. Video mostly
lands in `needs_probe`: judging a video means running ffprobe on it, and that means downloading
it, which a read-only projection must not do.

### `bench [--sample N]`

Downloads N pending video files (largest first) and encodes each at SVT-AV1 presets 3, 4 and 6,
plus one run with temporal filtering disabled, reporting size and VMAF for each. Use it to choose
`--preset` on evidence rather than folklore. Default `--sample 3`.

VMAF cannot see oversmoothing, so the output also reminds you to look at real frames and to diff
metadata with `exiftool -a -G1 <original> <converted>`.

### `run [flags]`

Converts files. **Without `--execute` it is a dry run**: it plans, stages and reports, but writes
nothing to the remote.

| Flag | Meaning |
| --- | --- |
| `--execute` | actually modify the remote |
| `--all` | required for an `--execute` run with no `--path` |
| `--allow-video` | permit the irreversible AV1 tier |
| `--limit N` | stop after N files |
| `--staging-dir DIR` | local scratch directory |
| `--budget-gb GB` | local staging budget (default: 70% of free space) |
| `--max-file-gb GB` | skip files whose staging reservation exceeds this (default: half the budget) |
| `--cloud-reserve-gb GB` | keep this much remote quota free as a margin (default 5) |
| `--reclaim-when-low-gb GB` | auto-purge trash below this much free remote space. `0` disables |
| `--net-concurrency N` | concurrent uploads and downloads (default 8) |
| `--cpu-permits N` | total CPU permits. `0` detects the core count |
| `--api-concurrency N` | concurrent lightweight remote API calls (default 4) |
| `--video-reserve-cores N` | cores video encoding leaves free so image jobs keep flowing (default 2) |
| `--order savings\|size\|path` | order files are picked up in (default `savings`) |
| `--min-video-secs SECONDS` | leave videos shorter than this alone (default `0`, meaning convert every length) |
| `--preset N` | SVT-AV1 preset. Lower is smaller and slower |
| `--allow-discard-corrupt` | let ffmpeg drop corrupt MPEG-TS packets. **This makes the remux lossy** |
| `--trash-policy keep\|purge-after-days\|purge-now` | when to empty the trash (default `keep`) |
| `--purge-after-days DAYS` | retention for `purge-after-days` (default 30) |

Work is scheduled per file with separate semaphores for disk, network, CPU and API calls, so a job
waiting for a core does not hold a network slot.

### `report`

Shows three quantities that must never be conflated: logical bytes removed from the content, the
actual free space the remote reports, and the bytes still sitting in the trash. With the default
`keep` policy the first goes up while the second does not move, which is expected and is why the
report keeps them apart.

### `verify [--sample N]`

Re-checks conversions that already happened. For byte-exact ones this is the real thing: the
converted file is fetched, rebuilt into its original, and hashed against the digest recorded
before anything was replaced. For the rest it confirms the replacement is present and the right
size, and reports `present` rather than `rebuilt` so the difference is never implied away. Exits
non-zero if anything fails.

### `restore <path> [--execute]`

Rebuilds one original from its converted form and puts it back. Only `byte-exact` conversions
qualify (in practice `jxl-from-jpeg`); anything else is refused with an explanation. Without
`--execute` it rebuilds and verifies without writing. It will not overwrite an existing file at
the original path, and it leaves the converted file in place for you to remove yourself.

For anything not byte-exact, the recovery path is the Filen trash, which is why `trash_policy`
defaults to `keep`.

### `dedup`

Reports files stored more than once, from the inventory alone. Read-only; deletes nothing.

### `cleanup [--execute]`

Empties the trash. Without `--execute` it reports what would be purged. With it, the originals are
gone for good and can no longer be restored from the Filen web app.

Purging hundreds of gigabytes is another single long call, so it shows the same spinner as `scan`.
Whether the file counter moves is up to the backend; the elapsed time always does.

It also compares the space the remote actually freed against what the ledger expected, and says so
when they diverge, since that usually means old file versions or an older trash are holding space
the ledger does not know about.

## Configuration

`~/.config/verified-recompress/config.toml`. Every key is optional. Precedence is CLI flags >
config file > auto-detection. Unknown keys are a hard error rather than a silent typo.

```toml
remote = "filen:"
staging_budget_gb = 50
max_file_gb = 20
cloud_reserve_gb = 5
reclaim_when_low_gb = 0
net_concurrency = 8
api_concurrency = 4
cpu_permits = 0
video_reserve_cores = 2
order = "savings"
min_video_secs = 0
paths = ["/Photos/2019", "/Camera"]
exclude = ["**/.thumbnails/**"]
trash_policy = "keep"
purge_after_days = 30
```

| Key | Default | Notes |
| --- | --- | --- |
| `remote` | `filen:` | any rclone remote |
| `staging_dir` | `$TMPDIR/verified-recompress` | see below |
| `staging_budget_gb` | 70% of free space | rejected if larger than what is actually free |
| `max_file_gb` | half the budget | files needing more are skipped as `too_large_for_budget` |
| `cloud_reserve_gb` | 5 | remote headroom kept free |
| `reclaim_when_low_gb` | disabled | `0` means disabled |
| `net_concurrency` | 8 | must be at least 1 |
| `api_concurrency` | 4 | must be at least 1 |
| `cpu_permits` | detected core count | `0` means detect |
| `video_reserve_cores` | 2 | must be less than `cpu_permits` |
| `order` | `savings` | `savings`, `size`, or `path`. `savings` ranks by projected saving, so a small file with a good ratio outranks a large one that converts poorly |
| `min_video_secs` | `0` | duration floor for the AV1 tier. `0` converts every length |
| `paths` | whole remote | CLI `--path` replaces this list entirely |
| `exclude` | none | CLI `--exclude` is appended to this list |
| `trash_policy` | `keep` | `keep`, `purge_after_days`, or `purge_now` |
| `purge_after_days` | 30 | only used by `purge_after_days` |

`--path` scoping is enforced at every stage, not just during `scan`, and prefixes are matched on
path-segment boundaries so `/photos` never picks up `/photos-backup`.

## Where state lives

The inventory is a SQLite ledger at `<staging_dir>/ledger.sqlite`. Since `staging_dir` defaults to
`$TMPDIR/verified-recompress`, **set it somewhere persistent** if you want the inventory to
survive a reboot:

```toml
staging_dir = "/var/lib/verified-recompress"
```

Local staging directories left behind by an interrupted run are swept at the start of the next
one, and files left claimed by a crashed run are returned to `pending`.

## What gets skipped, and why

Nothing is skipped silently. Every file gets either a recipe or a named reason, so "why is my
drive still full" is answerable from `plan` and `report`.

| Reason | Meaning |
| --- | --- |
| `already_optimal` | already HEIC/AVIF/WebP/JXL; re-encoding would only add loss |
| `lossy_no_gain` | lossy source with no lossless path to a smaller file (MP3, AAC, AAC-in-`.m4a`) |
| `too_small` | under 4 KiB, or zero bytes |
| `too_large_for_budget` | would not fit in the staging budget |
| `video_hdr` | HDR10, HLG or Dolby Vision; re-encoding loses the mastering metadata |
| `video_already_av1` | another pass would only stack generation loss |
| `video_low_bitrate` | already below 1.0 bits/pixel/second (roughly 1080p at 2 Mbps, 4K at 8 Mbps) |
| `video_too_short` | shorter than `min_video_secs`; off by default |
| `video_complex_structure` | multiple video streams or attachments a straight re-encode would mangle |
| `video_tier_disabled` | needs `--allow-video` |
| `needs_probe` | a video that `plan` cannot judge without downloading it |
| `unsupported` | nothing here is known to be improvable |
| `out_of_scope` | excluded by `--path` or `--exclude` |

Conversions projected to save less than 3% are not worth the upload, the risk, or the loss of the
original's exact bytes, and are dropped.

Two reasons depend on how the run was configured rather than on the file: `video_tier_disabled`
and `too_large_for_budget`. Those are automatically reconsidered on the next run when the settings
change, so turning on `--allow-video` does not silently do nothing to files it already passed
over.

## Status and caveats

Every stage is implemented and unit tested, but end-to-end validation against a live Filen drive
is still outstanding. Treat the first real run accordingly:

1. `preflight` on the target machine.
2. `scan` and `plan`, both read-only, to see the projected saving.
3. A narrow `run --path ... --limit 10 --execute` with `trash_policy = "keep"`.
4. `verify`, then `cleanup --execute` once you are satisfied.
5. `bench --sample 5` to settle on a preset before opening up `--allow-video`.

Other things worth knowing:

- AV1 keeps the source container (MOV stays MOV) so QuickTime metadata survives. MKV has no keyed
  atom slot for it.
- CPU budgeting relies on `taskset`, so it is enforced on Linux only.
- Video encoding is SVT-AV1 software only. NVENC is not used: at equal quality its output runs
  30-45% larger. The GPU is used for decoding during VMAF scoring, if available.
- `--allow-discard-corrupt` makes an MPEG-TS remux lossy. It exists for genuinely damaged
  captures, and it is off by default.
- The Filen trash counts against your quota until `cleanup --execute`.

## Development

```sh
cargo test          # 306 tests
cargo build --release
```

The crate is split into a library and a thin binary so integration tests exercise the same code
paths the CLI uses. Unit tests live beside the code; the integration suites are:

- `tests/roundtrip.rs`: real JPEG/PNG/WAV conversions, verified byte for byte
- `tests/video.rs`: TS remux, FFV1, and AV1 gate behaviour
- `tests/remote_parity.rs`: the two rclone access layers agree on paths and errors

Decision logic (`policy.rs`, `config.rs`, `preflight.rs`) is written as pure functions over
captured tool output, so the whole decision table is testable on a machine with none of the
encoders installed.

## License

Licensed under the GNU Affero General Public License v3.0 only. See [LICENSE](LICENSE).
