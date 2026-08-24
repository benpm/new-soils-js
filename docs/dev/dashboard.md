# The public test-recording dashboard

<http://34.41.84.40/> — a static page listing the screen recordings the test
suite produces, newest first, each naming the test that made it.

It is deliberately boring infrastructure: nginx serving a generated
`index.html` and a directory of MP4s. There is no application, no database and
nothing writable from outside, because the page only ever needs to be readable.

## Adding a recording

1. Record it (see the `#[ignore]`d recorders in `crates/soils-server/tests/`,
   and [`debug.md`](debug.md) for the OBS traps).
2. Add an entry to [`../../scripts/dashboard/videos.json`](../../scripts/dashboard/videos.json):

   ```json
   {
     "id": "2026-08-24_my-recording",
     "date": "2026-08-24",
     "title": "What it shows",
     "source": "recordings/obs/my_clip.mp4",
     "test": "crates/soils-server/tests/my_demo.rs",
     "test_name": "record_my_thing",
     "tags": ["physics", "two-client"],
     "blurb": "One paragraph. HTML is allowed; <code>…</code> reads well."
   }
   ```

   `id` is the served filename and `date` sorts the page, so lead with the
   date. `source` is relative to the repo root and need not be committed —
   `recordings/` is gitignored, and the dashboard is the place large media
   lives.

3. Deploy:

   ```sh
   python scripts/deploy_dashboard.py
   ```

The script re-encodes each source (H.264, ≤1280 px wide, CRF 30), cuts a poster
frame a third of the way in, regenerates `index.html`, uploads the tree and
reloads nginx. **It keeps whichever of the source and the re-encode is
smaller** — several of the OBS clips are already tuned harder than a generic
CRF 30 pass, and re-encoding them both inflated the file and stacked a second
generation of loss.

`--build` stops after generating `target/dashboard/`, which is worth doing
before a deploy if you changed the template.

### The `ref` field

Per-video links point at `blob/<ref>/<test>` on GitHub. `ref` currently names
the feature branch, because the recorders live there and `master` links would
404. **Flip it to `master` once that branch merges**, or the links break when
the branch is deleted.

## The instance

| | |
|---|---|
| Project | `new-soils` |
| Instance | `soils-dashboard`, `e2-micro`, `us-central1-a` |
| Image | Debian 12, 30 GB `pd-standard` |
| Address | `34.41.84.40` — **reserved static** (`soils-dashboard-ip`) |
| Firewall | `allow-http-soils`: `tcp:80` from `0.0.0.0/0`, tag `http-server` |
| Web root | `/var/www/soils`, owned by `www-data` |

`e2-micro` in `us-central1` with a 30 GB standard disk sits inside Google's
always-free tier; egress is the part that is billed, and a handful of MB per
visitor is not going to trouble the 1 GB/month free allowance. It is a public
HTTP server with no TLS, which is the right amount of infrastructure for
serving three MP4s of a voxel game and the wrong amount for anything else —
do not put anything private on it.

The address is reserved rather than ephemeral so the README link survives a
stop/start. A reserved address that is *not* attached to a running instance is
billed, so if the instance is deleted, release the address too.

## Recreating it

```sh
gcloud services enable compute.googleapis.com --project new-soils

gcloud compute firewall-rules create allow-http-soils --project new-soils \
  --allow tcp:80 --source-ranges 0.0.0.0/0 --target-tags http-server

gcloud compute instances create soils-dashboard --project new-soils \
  --zone us-central1-a --machine-type e2-micro \
  --image-family debian-12 --image-project debian-cloud \
  --boot-disk-size 30GB --boot-disk-type pd-standard \
  --tags http-server --metadata-from-file startup-script=scripts/dashboard/startup.sh

gcloud compute addresses create soils-dashboard-ip --project new-soils \
  --region us-central1 --addresses <the ephemeral IP it was given>
```

[`../../scripts/dashboard/startup.sh`](../../scripts/dashboard/startup.sh)
installs nginx and points it at `/var/www/soils`. It runs on every boot and is
idempotent.

Tearing it down:

```sh
gcloud compute instances delete soils-dashboard --zone us-central1-a --project new-soils
gcloud compute addresses delete soils-dashboard-ip --region us-central1 --project new-soils
gcloud compute firewall-rules delete allow-http-soils --project new-soils
```

## Gotchas

- **`gcloud` is not on `PATH`** in a default Windows install. It lives at
  `%LOCALAPPDATA%\Google\Cloud SDK\google-cloud-sdk\bin`; the deploy script
  falls back to that location.
- **gcloud drives PuTTY on Windows**, and PuTTY stops dead on an uncached host
  key waiting for a keypress no script will supply. Every `ssh`/`scp` call
  passes `--strict-host-key-checking=no`.
- **`pscp --recurse` copies the directory *into* the destination.** The
  destination is therefore the parent (`/tmp/`, giving `/tmp/dashboard`), not
  `/tmp/dashboard` — which quietly yields `/tmp/dashboard/dashboard` and a
  dashboard that 404s every video while `index.html` still loads.
- **Seeking needs range requests.** nginx serves them by default; the site
  config only adds `Accept-Ranges` explicitly so a misconfiguration is visible.
  Verify with `curl -o /dev/null -w '%{http_code}' -H 'Range: bytes=0-1023'`,
  which must answer **206**, not 200.
