# Getting started

## Install with Docker

```sh
git clone <this repo> xmusic && cd xmusic
```

Edit [docker-compose.yml](../docker-compose.yml):

```yaml
services:
  xmusic:
    build: .
    user: "1000:1000"            # the owner of your music files
    ports: ["8337:8337"]
    environment:
      TZ: UTC                    # time zone for the scan schedule, e.g. Europe/Lisbon
    volumes:
      - ./data:/data             # database: users, settings, undo log
      - /path/to/music:/music    # your library
```

Then `docker compose up --build -d`. Run the container as the user that owns your music, so tags, `cover.jpg`
and `.lrc` files keep their ownership. The image (about 70 MB, distroless, no shell) includes a static `fpcalc` (Chromaprint) for AcoustID
fingerprints. It is built for x86_64.

## Run without Docker

```sh
cargo run --release
```

The server listens on port 8337 and keeps its data in `./data`.

| Variable | Default | Meaning |
|----------|---------|---------|
| `XMUSIC_PORT` | `8337` | HTTP port |
| `XMUSIC_DATA` | `./data` (`/data` in the image) | Folder for the SQLite database |

## First login

Open <http://localhost:8337>. The first visit asks you to create the admin account. Passwords are stored as
argon2 hashes and login attempts are throttled per IP.

Forgot the password? Run this on the host or inside the container; it reads the new password from stdin:

```sh
docker compose exec -T xmusic xmusic reset-password admin <<< "new-password"
```

More users can be added on the **Users** page. Admins can change settings, libraries and users; regular users
can browse and review.

## Add a library

Go to **Settings → Libraries**, enter a name and a path *as the container sees it* (for example `/music` or
`/music/Rock`) and add it. A scan starts right away. Libraries can be switched off without forgetting them, and
removing one never touches files.

![Settings](img/settings.png)

Subfolders `CD1/`, `Disc 2/` and similar are treated as part of their parent album.

## Your first run

1. **Jobs → Run now** scans the library, matches albums that have no MusicBrainz release id, and applies the
   confident ones. It can take a while on a large library; the progress bar shows the phase and an ETA.
2. Doubtful albums land in **Review**. See [Matching and review](matching-and-review.md).
3. Set a contact e-mail in **Settings → General**. MusicBrainz asks for one in the User-Agent.
4. Optional: an AcoustID key (free at [acoustid.org](https://acoustid.org): register an *application*; the user
   key on your profile is rejected) lets xmusic identify files by fingerprint when their tags are too poor to
   search. **Settings → General → Test key** checks the key and `fpcalc`.

## Updating

```sh
git pull && docker compose up --build -d
```

Database changes are migrated automatically on start. Some upgrades re-read every file once on the next scan.
