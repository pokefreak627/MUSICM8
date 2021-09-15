use futures::StreamExt;
use songbird::{
    input::{Input, Restartable},
    tracks::TrackHandle,
    Songbird,
};
use std::{collections::HashMap, error::Error, sync::Arc};
use tokio::{spawn, sync::RwLock};
use twilight_gateway::{Cluster, Event, Intents};
use twilight_http::Client as HttpClient;
use twilight_model::{channel::Message, gateway::payload::MessageCreate, id::GuildId};
use twilight_standby::Standby;

type State = Arc<StateRef>;

#[derive(Debug)]
struct StateRef {
    cluster: Cluster,
    http: HttpClient,
    trackdata: RwLock<HashMap<GuildId, TrackHandle>>,
    songbird: Songbird,
    standby: Standby,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    tracing_subscriber::fmt::init();

    let (mut events, state) = {
        let token = include_str!("../.token");

        let http = HttpClient::new(token.to_string());
        let user_id = http.current_user().exec().await?.model().await?.id;

        let intents = Intents::GUILD_MESSAGES | Intents::GUILD_VOICE_STATES;
        let (cluster, events) = Cluster::new(token, intents).await?;
        cluster.up().await;

        let songbird = Songbird::twilight(cluster.clone(), user_id);

        (
            events,
            Arc::new(StateRef {
                cluster,
                http,
                trackdata: Default::default(),
                songbird,
                standby: Standby::new(),
            }),
        )
    };

    while let Some((_, event)) = events.next().await {
        state.standby.process(&event);
        state.songbird.process(&event).await;

        if let Event::MessageCreate(msg) = event {
            if msg.guild_id.is_none() || !msg.content.starts_with("j/") {
                continue;
            }

            match msg.content.splitn(2, ' ').next() {
                Some("j/join") => spawn(join(msg.0, Arc::clone(&state))),
                Some("j/play") => spawn(play(msg.0, Arc::clone(&state))),
                Some("j/leave") => spawn(leave(msg.0, Arc::clone(&state))),
                Some("j/stop") => spawn(stop(msg.0, Arc::clone(&state))),

                _ => continue,
            };
        }
    }

    Ok(())
}
async fn join(msg: Message, state: State) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    state
        .http
        .create_message(msg.channel_id)
        .content("What's the channel ID you want me to join?")?
        .exec()
        .await?;

    let author_id = msg.author.id;
    let msg = state
        .standby
        .wait_for_message(msg.channel_id, move |new_msg: &MessageCreate| {
            new_msg.author.id == author_id
        })
        .await?;
    let channel_id = msg.content.parse::<u64>()?;

    let guild_id = msg.guild_id.ok_or("Can't join a non-guild channel.")?;

    let (_handle, success) = state.songbird.join(guild_id, channel_id).await;

    let content = match success {
        Ok(()) => format!("Joined <#{}>!", channel_id),
        Err(e) => format!("Failed to join <#{}>! Why: {:?}", channel_id, e),
    };

    state
        .http
        .create_message(msg.channel_id)
        .content(&content)?
        .exec()
        .await?;

    Ok(())
}

async fn leave(msg: Message, state: State) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    tracing::debug!(
        "leave command in channel {} by {}",
        msg.channel_id,
        msg.author.name
    );

    let guild_id = msg.guild_id.unwrap();

    state.songbird.leave(guild_id).await?;

    state
        .http
        .create_message(msg.channel_id)
        .content("Left the channel")?
        .exec()
        .await?;

    Ok(())
}

async fn play(msg: Message, state: State) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    tracing::debug!(
        "play command in channel {} by {}",
        msg.channel_id,
        msg.author.name
    );
    state
        .http
        .create_message(msg.channel_id)
        .content("What's the URL of the audio to play?")?
        .exec()
        .await?;

    let author_id = msg.author.id;
    let msg = state
        .standby
        .wait_for_message(msg.channel_id, move |new_msg: &MessageCreate| {
            new_msg.author.id == author_id
        })
        .await?;

    let guild_id = msg.guild_id.unwrap();

    match songbird::ytdl(msg.content.trim()).await {
        Ok(song) => {
            let input = Input::from(song);

            let content = format!(
                "Playing **{:?}** by **{:?}**",
                input
                    .metadata
                    .track
                    .as_ref()
                    .unwrap_or(&"<UNKNOWN>".to_string()),
                input
                    .metadata
                    .artist
                    .as_ref()
                    .unwrap_or(&"<UNKNOWN>".to_string()),
            );

            state
                .http
                .create_message(msg.channel_id)
                .content(&content)?
                .exec()
                .await?;

            if let Some(call_lock) = state.songbird.get(guild_id) {
                let mut call = call_lock.lock().await;
                let handle = call.play_source(input);

                let mut store = state.trackdata.write().await;
                store.insert(guild_id, handle);
            }
        }
        Err(e) => {
            state
                .http
                .create_message(msg.channel_id)
                .content(&format!("error {}", e))?
                .exec()
                .await?;
        }
    }

    Ok(())
}

async fn stop(msg: Message, state: State) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    tracing::debug!(
        "stop command in channel {} by {}",
        msg.channel_id,
        msg.author.name
    );

    let guild_id = msg.guild_id.unwrap();

    if let Some(call_lock) = state.songbird.get(guild_id) {
        let mut call = call_lock.lock().await;
        let _ = call.stop();
    }

    state
        .http
        .create_message(msg.channel_id)
        .content("Stopped the track")?
        .exec()
        .await?;

    Ok(())
}
