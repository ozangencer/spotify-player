# spotify_player — Development Mode fork

A fork of [aome510/spotify-player](https://github.com/aome510/spotify-player) (v0.24.1), a terminal
Spotify client written in Rust. This fork exists for one reason: **to keep the app usable with your
own Spotify Developer client ID** after Spotify's 2025/2026 API changes. Branch: `devmode-ratelimit-fixes`.

If the upstream app works for you, use upstream. If you see `429 Too Many Requests` all day, or
`missing field popularity` after switching to your own client ID, this fork is for you.

## Why this fork differs from upstream

Two things happened in a row.

**1. The bundled client ID is rate limited around the clock.** Upstream ships with a shared client
ID (ncspot's). Spotify's quota is per application, so every ncspot and spotify-player user in the
world shares one 30-second window. In September 2026, measured from an idle machine, 10 of 12
requests were rejected with `429` and `Retry-After` between 1 and 21 seconds. Search needs four
successful requests in a row, so it almost never completed
([upstream #974](https://github.com/aome510/spotify-player/issues/974)).

**2. A client ID you register today is in "Development Mode".** Spotify's
[February 2026 rules](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
apply to it, and upstream 0.24.1 breaks on all of them:

| Development Mode change | Effect on upstream 0.24.1 |
|---|---|
| Fields removed (`popularity`, `followers`, `genres`, `available_markets`, …) | `json parse error: missing field popularity`; search, playback and library pages fail to parse |
| Playlist `tracks` renamed to `items`, `/playlists/{id}/tracks` → `/items` | Playlists fail to load |
| Playlist contents only returned for playlists you own | Other people's playlists show no tracks |
| `/artists/{id}/top-tracks`, `/artists/{id}/related-artists` removed | Artist page fails |
| Batch endpoints (`/tracks?ids=…`) removed | Radio fails |
| Browse endpoints removed | Browse page fails |
| Library endpoints replaced by `/me/library` | Like / unlike / follow fail |
| Search capped at 10 results per page | Fewer results |
| Per-endpoint **daily** quotas with `Retry-After` of up to 24 h | A page that keeps retrying every 5 s burns a whole day's quota in minutes |

## What this fork does about it

The fork keeps the Web API for what still works well (your library, search, playback control,
likes) and uses **librespot's metadata API** (the same internal API the official clients use, no
Web API quota) for content Spotify no longer serves to Development Mode apps.

| Feature | Upstream 0.24.1 | This fork |
|---|---|---|
| Search | 6 parallel requests, any failure blanks the page | 4 sequential, partial failures tolerated, dev-mode limit (10) |
| Rate limiting (`429`) | No retry, page stays on "Loading…" | Exponential backoff; long `Retry-After` puts the request on hold instead of retrying |
| Playback polling at track end | 100 ms request storm while failing | Throttled to 2 s |
| Artist page | Web API (top tracks + albums + related) | librespot: name, top tracks, albums, singles; up to 40 tracks (search fill); related artists derived from the artist's radio; parts fetched concurrently (~3 s) |
| Playlists you don't own | Fails (no items) | Tracks fetched through librespot |
| Radio | Fails (batch endpoint) | Track details through librespot |
| Like / unlike / follow / playlist follow | Old endpoints (403) | `/me/library` endpoints |
| rspotify models | Strict (missing field = error) | Vendored `rspotify` / `rspotify-model` 0.15.3 with serde defaults and `items` / `item` aliases (`vendor/`, wired via `[patch.crates-io]`) |
| Page limits | Fixed 50 | Falls back to 10 when an endpoint rejects the limit |
| Search page keys | Input box captures every key | `Esc` leaves the input so shortcuts work again |

Everything else (UI, keymaps, config, CLI, daemon) is unchanged from upstream. See
[README.upstream.md](README.upstream.md) for the full upstream documentation and
`git log v0.24.1..HEAD` for the individual changes.

## Installation

Build from source (the feature set below matches the Homebrew formula):

```sh
git clone -b devmode-ratelimit-fixes https://github.com/ozangencer/spotify-player.git
cd spotify-player
cargo install --path spotify_player --features image,notify --target-dir target
```

This installs `spotify_player` into `~/.cargo/bin`. If you also have the Homebrew package, uninstall
it or make sure `~/.cargo/bin` comes first in `PATH`.

## Setting up your own client ID

1. Create an app on the [Spotify developer dashboard](https://developer.spotify.com/dashboard)
   (Web API enabled, redirect URI `http://127.0.0.1:8989/login`). A Premium account is required.
2. In `~/.config/spotify-player/app.toml`:
   ```toml
   client_id = "<your client id>"
   ```
3. Refresh the cached token:
   ```sh
   rm ~/.cache/spotify-player/user_client_token.json && spotify_player authenticate
   ```

## Playback

Spotify changed its audio key delivery in December 2025 and librespot's integrated player no longer
works for many accounts (`error audio key 0 1`, see
[librespot #1649](https://github.com/librespot-org/librespot/issues/1649)). If that is your case,
disable the integrated player and use the official Spotify app as the playback device; the TUI then
acts as a remote:

```toml
enable_streaming = "Never"
```

Press `D` in the app to pick the device once.

## Known limitations

- The Browse page does not work (Spotify removed the endpoints for Development Mode apps).
- Search returns at most 10 results per category.
- Related artists are an approximation built from the artist's radio, not Spotify's own list.
- Development Mode quotas are per endpoint and per day. The fork avoids the Web API where it can and
  honors `Retry-After`, but a blocked endpoint stays blocked until Spotify's deadline passes.
- The fork tracks upstream v0.24.1; rebase the branch when a new upstream release appears.

## License

MIT, same as upstream.
