mod model;
mod transcriber;

use std::{collections::HashMap, sync::Arc};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use futures::stream::StreamExt;
use gstreamer::{
  prelude::{ElementExt, GObjectExtManualGst, GstBinExt},
  Bin, ElementFactory, GhostPad,
};
use lib_gst_meet::{
  init_tracing, Authentication, ColibriMessage, Connection, JitsiConference, JitsiConferenceConfig,
  MediaType,
};
use structopt::StructOpt;
use tokio::{
  signal::ctrl_c,
  sync::{mpsc, Mutex},
  task,
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, trace};

use crate::{
  model::{Participant, TranscriptionEvent, TranscriptionResult},
  transcriber::{aws::AwsTranscriber, Transcriber},
};

#[derive(Debug, Clone, StructOpt)]
#[structopt(
  name = "regasi",
  about = "Perform speech-to-text on Jitsi Meet conferences for transcription and captioning."
)]
struct Opt {
  #[structopt(long)]
  web_socket_url: String,
  #[structopt(long)]
  xmpp_domain: String,
  #[structopt(long)]
  username: Option<String>,
  #[structopt(long)]
  password: Option<String>,
  #[structopt(long)]
  room_name: String,
  #[structopt(long)]
  muc_domain: Option<String>,
  #[structopt(long)]
  focus_jid: Option<String>,
  #[structopt(long, default_value)]
  region: String,
  #[structopt(long)]
  transcriber: String,
  #[structopt(long)]
  transcriber_region: Option<String>,
  #[structopt(long)]
  live: bool,
  #[structopt(long)]
  save: Option<String>,
  #[structopt(short, long, parse(from_occurrences))]
  verbose: u8,
}

fn init_gstreamer() -> Result<()> {
  trace!("starting gstreamer init");
  gstreamer::init()?;
  trace!("finished gstreamer init");
  Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
  let opt = Opt::from_args();

  init_tracing(match opt.verbose {
    0 => tracing::Level::INFO,
    1 => tracing::Level::DEBUG,
    _ => tracing::Level::TRACE,
  });
  glib::log_set_default_handler(glib::rust_log_handler);

  init_gstreamer()?;

  let client_connection = {
    let (connection, background) = Connection::new(
      &opt.web_socket_url,
      &opt.xmpp_domain,
      match (opt.username.as_ref(), opt.password.as_ref()) {
        (Some(username), Some(password)) => Authentication::Plain {
          username: username.clone(),
          password: password.clone(),
        },
        (None, None) => Authentication::Anonymous,
        _ => panic!("if username or password are provided, both must be provided"),
      },
    )
    .await
    .context("failed to connect")?;
    tokio::spawn(background);
    connection.connect().await?;
    connection
  };

  // TODO: implement brewery MUC support
  // let service_connection = {
  //   let (connection, background) =
  //     Connection::new(
  //       &opt.web_socket_url,
  //       &opt.xmpp_domain,
  //       match (opt.username.as_ref(), opt.password.as_ref()) {
  //         (Some(username), Some(password)) => Authentication::Plain { username: username.clone(), password: password.clone() },
  //         (None, None) => Authentication::Anonymous,
  //         _ => panic!("if username or password are provided, both must be provided"),
  //       },
  //     )
  //     .await
  //     .context("failed to connect")?;
  //   tokio::spawn(background);
  //   connection.connect().await?;
  //   connection
  // };

  let room_jid = format!(
    "{}@{}",
    opt.room_name,
    opt
      .muc_domain
      .clone()
      .unwrap_or_else(|| { format!("conference.{}", opt.xmpp_domain) }),
  );

  let focus_jid = opt
    .focus_jid
    .clone()
    .unwrap_or_else(|| format!("focus@auth.{}/focus", opt.xmpp_domain,));

  let config = JitsiConferenceConfig {
    muc: room_jid.parse()?,
    focus: focus_jid.parse()?,
    nick: "Transcriber".to_owned(),
    region: opt.region.clone(),
    video_codec: "vp8".to_owned(),
    extra_muc_features: vec!["http://jitsi.org/protocol/jigasi".to_owned()],
  };

  let main_loop = glib::MainLoop::new(None, false);

  let conference = JitsiConference::join(connection, main_loop.context(), config)
    .await
    .context("failed to join conference")?;

  let (save_tx, save_rx) = mpsc::channel(64);
  let (live_tx, live_rx) = mpsc::channel(8);

  let transcriber = opt.transcriber.clone();
  let transcriber_region = opt.transcriber_region.clone();
  let region = opt.region.clone();
  let save = opt.save.is_some();

  let transcribers = Arc::new(Mutex::new(HashMap::new()));

  {
    let transcribers = transcribers.clone();
    let save_tx = save_tx.clone();

    conference
      .on_participant(move |conference, participant| {
        let transcribers = transcribers.clone();
        let transcriber = transcriber.clone();
        let transcriber_region = transcriber_region.clone();
        let live_tx = live_tx.clone();
        let save_tx = save_tx.clone();
        Box::pin(async move {
          info!("New participant: {:?}", participant);

          let participant_id = participant.muc_jid.resource.clone();
          let transcription_participant = Participant {
            id: participant_id.clone(),
            name: participant.nick.unwrap_or_else(|| "Someone".to_owned()),
            ..Default::default()
          };

          let transcriber: Box<dyn Transcriber + Send + Sync> = match transcriber.as_str() {
            "aws" => Box::new(AwsTranscriber::new(
              transcriber_region.as_deref(),
              &transcription_participant,
              live_tx,
            )),
            other => bail!("invalid transcriber name: {}", other),
          };

          if save {
            save_tx
              .send(TranscriptionEvent::Join {
                timestamp: Utc::now(),
                participant: transcription_participant.clone(),
              })
              .await?;
          }

          let bin = Bin::new(Some(&format!("participant_{}", participant_id)));

          let queue = ElementFactory::make("queue", None)?;
          bin.add(&queue)?;
          let sink_pad = queue
            .static_pad("sink")
            .context("queue element missing sink pad")?;
          bin.add_pad(&GhostPad::with_target(Some("audio"), &sink_pad)?)?;

          let transcriber_element = transcriber.make_element()?;
          bin.add(&transcriber_element)?;
          queue.link(&transcriber_element)?;

          conference.add_bin(&bin).await?;

          transcribers
            .lock()
            .await
            .insert(participant_id.clone(), transcriber);

          Ok(())
        })
      })
      .await;
  }

  {
    let transcribers = transcribers.clone();
    let conference = conference.clone();
    let save_tx = save_tx.clone();
    conference
      .on_participant_left(move |conference, participant| {
        let transcribers = transcribers.clone();
        let save_tx = save_tx.clone();
        Box::pin(async move {
          info!("Participant left: {:?}", participant);

          let participant_id = participant.muc_jid.resource;

          let pipeline = conference.pipeline().await?;
          if let Some(bin) = pipeline.by_name(&format!("participant_{}", participant_id)) {
            pipeline.remove(&bin)?;
          }

          transcribers.lock().await.remove(&participant_id);

          if save {
            save_tx
              .send(TranscriptionEvent::Leave {
                timestamp: Utc::now(),
                participant: Participant {
                  id: participant_id,
                  ..Default::default()
                },
              })
              .await?;
          }

          Ok(())
        })
      })
      .await;
  }

  let live = opt.live;

  {
    let conference = conference.clone();
    tokio::spawn(async move {
      let mut stream = ReceiverStream::new(live_rx);
      let mut transcription: Option<TranscriptionResult> = None;
      while let Some(event) = stream.next().await {
        if live {
          conference
            .send_json_message(&TranscriptionResult {
              r#type: "transcription-result".to_owned(),
              event: event.clone(),
            })
            .await?;
        }
        if save && !event.is_interim() {
          save_tx.send(event).await?;
        }
      }
      Ok::<_, anyhow::Error>(())
    });
  }

  if let Some(path) = opt.save {
    tokio::spawn(async move {
      let mut stream = ReceiverStream::new(save_rx);
      while let Some(event) = stream.next().await {}
    });
  }

  // Signal that our audio and video are muted.
  conference.set_muted(MediaType::Audio, true).await?;
  conference.set_muted(MediaType::Video, true).await?;

  // Request that no video is sent to us.
  conference
    .send_colibri_message(ColibriMessage::ReceiverVideoConstraints {
      last_n: Some(0),
      selected_endpoints: None,
      on_stage_endpoints: None,
      default_constraints: None,
      constraints: None,
    })
    .await?;

  conference
    .set_pipeline_state(gstreamer::State::Playing)
    .await?;

  tokio::spawn(async move {
    ctrl_c().await.unwrap();
    std::process::exit(0);
  });

  task::spawn_blocking(move || main_loop.run()).await?;

  Ok(())
}
