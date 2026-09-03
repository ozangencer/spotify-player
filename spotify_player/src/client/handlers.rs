use std::time::{Duration, Instant};

use anyhow::Context;
use rspotify::model::Id;
use tracing::Instrument;

use crate::{
    config,
    state::{ContextId, ContextPageType, ContextPageUIState, PageState, PlayableId, SharedState},
};

use crate::utils::map_join;

use super::ClientRequest;

struct PlayerEventHandlerState {
    get_context_timer: Instant,
    last_playback_refresh_timer: Instant,
    /// Last time a playback/queue poll was triggered by the 100ms event watcher.
    /// Used to avoid re-sending the same request every tick while it keeps failing.
    last_event_poll_timer: Option<Instant>,
}

/// Minimum interval between two playback/queue polls triggered by the event watcher.
const EVENT_POLL_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum number of retries for a request rejected with `429 Too Many Requests`.
const RATE_LIMIT_MAX_RETRIES: u32 = 4;
/// Base delay for the exponential backoff between rate-limit retries (2s, 4s, 8s, 16s).
const RATE_LIMIT_BASE_DELAY: Duration = Duration::from_secs(2);

/// starts the client's request handler
pub async fn start_client_handler(
    state: &SharedState,
    client: &super::AppClient,
    client_sub: &flume::Receiver<ClientRequest>,
) {
    while let Ok(request) = client_sub.recv_async().await {
        let state = state.clone();
        let client = client.clone();
        let span = tracing::info_span!("client_request", request = ?request);

        tokio::task::spawn(
            async move {
                // Requests that are already re-issued periodically by other mechanisms
                // (playback polling, context page refresh) are not retried here, otherwise
                // retries would pile up while being rate-limited.
                let retryable = match &request {
                    ClientRequest::GetCurrentPlayback | ClientRequest::GetCurrentUserQueue => false,
                    ClientRequest::GetContext(id) => matches!(id, ContextId::Tracks(_)),
                    _ => true,
                };

                let mut attempt = 0u32;
                loop {
                    match client.handle_request(&state, request.clone()).await {
                        Ok(()) => break,
                        Err(err) => {
                            let rate_limited = format!("{err:#}").contains("429");
                            if retryable && rate_limited && attempt < RATE_LIMIT_MAX_RETRIES {
                                let delay = RATE_LIMIT_BASE_DELAY * 2u32.pow(attempt);
                                attempt += 1;
                                tracing::warn!(
                                    "Rate limited by Spotify (429), retrying in {delay:?} (attempt {attempt}/{RATE_LIMIT_MAX_RETRIES})"
                                );
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            tracing::error!("Failed to handle client request: {err:#}");
                            break;
                        }
                    }
                }
            }
            .instrument(span),
        );
    }
}

/// Interval between background session-validity checks.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub async fn start_session_watcher(state: SharedState, client: super::AppClient) {
    let mut interval = tokio::time::interval(SESSION_CHECK_INTERVAL);
    // If a check ever runs long (e.g. a slow reconnect), skip missed ticks
    // rather than firing them back-to-back.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        if let Err(err) = client.check_valid_session(&state).await {
            tracing::error!("Failed to check/reconnect the client's session: {err:#}");
        }
    }
}

fn handle_playback_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    // throttle: while a previous poll is still failing (e.g. rate-limited), don't
    // re-send the same request on every 100ms tick
    if handler_state
        .last_event_poll_timer
        .is_some_and(|t| t.elapsed() < EVENT_POLL_MIN_INTERVAL)
    {
        return Ok(());
    }

    let player = state.player.read();
    let (playback, id, duration) = match (
        player.buffered_playback.as_ref(),
        player.currently_playing(),
    ) {
        (Some(playback), Some(rspotify::model::PlayableItem::Track(track))) => (
            playback,
            PlayableId::Track(track.id.clone().expect("null track_id")),
            track.duration,
        ),
        (Some(playback), Some(rspotify::model::PlayableItem::Episode(episode))) => (
            playback,
            PlayableId::Episode(episode.id.clone()),
            episode.duration,
        ),
        _ => return Ok(()),
    };

    if let Some(progress) = player.playback_progress() {
        // update the playback when the current track ends
        if progress >= duration && playback.is_playing {
            client_pub.send(ClientRequest::GetCurrentPlayback)?;
            handler_state.last_event_poll_timer = Some(Instant::now());
        }
    }

    if let Some(queue) = player.queue.as_ref() {
        // queue needs to be updated if its playing track is different from actual playback's playing track
        if let Some(queue_track) = queue.currently_playing.as_ref() {
            if queue_track.id().expect("null track_id") != id {
                client_pub.send(ClientRequest::GetCurrentUserQueue)?;
                handler_state.last_event_poll_timer = Some(Instant::now());
            }
        }
    } else {
        client_pub.send(ClientRequest::GetCurrentUserQueue)?;
        handler_state.last_event_poll_timer = Some(Instant::now());
    }

    Ok(())
}

fn handle_page_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    match state.ui.lock().current_page_mut() {
        PageState::Context {
            id,
            context_page_type,
            state: page_state,
        } => {
            let expected_id = match context_page_type {
                ContextPageType::Browsing(context_id) => Some(context_id.clone()),
                ContextPageType::CurrentPlaying => state.player.read().playing_context_id(),
            };

            let new_id = if *id == expected_id {
                false
            } else {
                // update the context state and request new data when moving to a new context page
                tracing::info!("Current context ID ({:?}) is different from the expected ID ({:?}), update the context state", id, expected_id);

                *id = expected_id;

                // update the UI page state based on the context's type
                match id {
                    Some(id) => {
                        *page_state = Some(match id {
                            ContextId::Album(_) => ContextPageUIState::new_album(),
                            ContextId::Artist(_) => ContextPageUIState::new_artist(),
                            ContextId::Playlist(_) => ContextPageUIState::new_playlist(),
                            ContextId::Tracks(_) => ContextPageUIState::new_tracks(),
                            ContextId::Show(_) => ContextPageUIState::new_show(),
                        });
                    }
                    None => {
                        *page_state = None;
                    }
                }
                true
            };

            // request new context's data if not found in memory
            // To avoid making too many requests, only request if context id is changed
            // or it's been a while since the last request.
            if let Some(id) = id {
                if !matches!(id, ContextId::Tracks(_))
                    && !state.data.read().caches.context.contains_key(&id.uri())
                    && (new_id
                        || handler_state.get_context_timer.elapsed() > Duration::from_secs(5))
                {
                    client_pub.send(ClientRequest::GetContext(id.clone()))?;
                    handler_state.get_context_timer = Instant::now();
                }
            }
        }

        PageState::Lyrics {
            track_uri,
            track,
            artists,
        } => {
            if let Some(rspotify::model::PlayableItem::Track(current_track)) =
                state.player.read().currently_playing()
            {
                if current_track.name != *track {
                    if let Some(id) = &current_track.id {
                        tracing::info!("Currently playing track \"{}\" is different from the track \"{track}\" shown up in the lyrics page. Fetching new track's lyrics...", current_track.name);
                        track.clone_from(&current_track.name);
                        *artists = map_join(&current_track.artists, |a| &a.name, ", ");
                        *track_uri = id.uri();
                        client_pub.send(ClientRequest::GetLyrics {
                            track_id: id.clone_static(),
                        })?;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn handle_player_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    handle_page_change_event(state, client_pub, handler_state)
        .context("handle page change event")?;
    handle_playback_change_event(state, client_pub, handler_state)
        .context("handle playback change event")?;

    Ok(())
}

/// Starts event watcher listening to events and making update requests to the client if needed
pub fn start_player_event_watcher(state: &SharedState, client_pub: &flume::Sender<ClientRequest>) {
    let configs = config::get_config();

    let refresh_duration = Duration::from_millis(100);
    let playback_refresh_duration =
        Duration::from_millis(configs.app_config.playback_refresh_duration_in_ms);
    let mut handler_state = PlayerEventHandlerState {
        get_context_timer: Instant::now(),
        last_playback_refresh_timer: Instant::now(),
        last_event_poll_timer: None,
    };

    loop {
        // periodically refresh the playback state (if enabled in config)
        if configs.app_config.playback_refresh_duration_in_ms > 0
            && handler_state.last_playback_refresh_timer.elapsed() >= playback_refresh_duration
        {
            client_pub
                .send(ClientRequest::GetCurrentPlayback)
                .unwrap_or_default();
            handler_state.last_playback_refresh_timer = Instant::now();
        }

        if let Err(err) = handle_player_event(state, client_pub, &mut handler_state) {
            tracing::error!("Encounter error when handling player event: {err:#}");
        }

        std::thread::sleep(refresh_duration);
    }
}
