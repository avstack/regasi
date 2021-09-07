mod model;

use std::{io, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use aws_sigv4::http_request::{calculate_signing_headers, SignableBody};
use aws_types::credentials::ProvideCredentials;
use bytes::{BufMut, Bytes, BytesMut};
use chrono::Utc;
use futures::stream::{self, StreamExt, TryStreamExt};
use glib::{Cast, ObjectExt};
use gstreamer::{Bin, Buffer, Caps, Element, ElementFactory, FlowError, FlowSuccess, GhostPad, PadDirection, PadPresence, PadTemplate, prelude::{ElementExt, ElementExtManual, GObjectExtManualGst, GstBinExt}};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use hyper::{Body, Client, Request, Version};
use hyper_rustls::HttpsConnector;
use smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder, Header, HeaderValue, Message};
use tokio::{sync::{mpsc, Mutex}, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

use crate::{
  model::{Participant, Transcript, TranscriptionEvent},
  transcriber::{aws::model::AwsTranscriptEvent, Transcriber},
};

pub(crate) struct AwsTranscriber {
  audio_tx: mpsc::Sender<Bytes>,
  join_handle: JoinHandle<()>,
}

impl AwsTranscriber {
  async fn transcribe(
    region: String,
    participant: Participant,
    audio_rx: mpsc::Receiver<Bytes>,
    transcription_tx: mpsc::Sender<TranscriptionEvent>,
  ) -> Result<()> {
    let connector = HttpsConnector::with_native_roots();
    let client = Client::builder()
      .http2_only(true)
      .build(connector);

    let creds_provider = aws_config::default_provider::credentials::default_provider().await;
    let creds = creds_provider.provide_credentials().await?;

    let last_sig = Arc::new(Mutex::new(String::new()));
    let mut last_sig_locked = last_sig.lock().await;

    let (audio_stream, abort_audio_stream) = {
      let last_sig = last_sig.clone();
      let region = region.clone();
      let creds = creds.clone();

      stream::abortable(
        ReceiverStream::new(audio_rx)
          .map(move |audio| {
            debug!("encoding {} bytes of audio to eventstream message", audio.len());
            Ok(Some(
              Message::new(audio)
                .add_header(Header::new(":content-type", HeaderValue::String("application/octet-stream".into())))
                .add_header(Header::new(":message-type", HeaderValue::String("event".into())))
                .add_header(Header::new(":event-type", HeaderValue::String("AudioEvent".into())))
            ))
          })
          .chain(stream::once(async move {
            info!("reached end of audio transmission stream");
            Ok(None)
          }))
          .and_then(move |maybe_payload| {
            let last_sig = last_sig.clone();
            let region = region.clone();
            let creds = creds.clone();

            async move {
              let message = {
                let mut last_sig_locked = last_sig.lock().await;
                let signing_params = aws_sigv4::event_stream::SigningParams {
                  access_key: creds.access_key_id(),
                  secret_key: creds.secret_access_key(),
                  security_token: None,
                  region: &region,
                  service_name: "transcribe",
                  date_time: Utc::now(),
                  settings: (),
                };
                let (message, sig) = if let Some(payload) = maybe_payload {
                  debug!("signing eventstream message: {:?}", payload);
                  debug!("prev signature: {}", last_sig_locked);
                  aws_sigv4::event_stream::sign_message(
                    &payload,
                    &*last_sig_locked,
                    &signing_params,
                  ).into_parts()
                }
                else {
                  debug!("signing final eventstream message");
                  debug!("prev signature: {}", last_sig_locked);
                  aws_sigv4::event_stream::sign_empty_message(
                    &*last_sig_locked,
                    &signing_params,
                  ).into_parts()
                };
                debug!("new signature: {}", sig);
                *last_sig_locked = sig;
                message
              };
              let mut buf = Vec::new();
              message.write_to(&mut buf)?;
              Ok::<_, anyhow::Error>(buf)
            }
          })
      )
    };

    let host = format!("transcribestreaming.{}.amazonaws.com", region);
    let mut req = Request::builder()
      .version(Version::HTTP_2)
      .method("POST")
      .uri(format!("https://{}/stream-transcription", host))
      .header("content-type", "application/vnd.amazon.eventstream")
      .header("x-amzn-transcribe-enable-partial-results-stabilization", "true")
      .header("x-amzn-transcribe-partial-results-stability", "high")
      .header("x-amzn-transcribe-language-code", "en-US")
      .header("x-amzn-transcribe-media-encoding", "ogg-opus")
      .header("x-amzn-transcribe-sample-rate", "48000")
      .header("x-amz-target", "com.amazonaws.transcribe.Transcribe.StartStreamTranscription")
      .body(Body::wrap_stream(audio_stream))?;
    
    let mut settings = aws_sigv4::http_request::SigningSettings::default();
    settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
    let (sig_headers, sig) = 
      calculate_signing_headers(
        &req,
        SignableBody::Precomputed("STREAMING-AWS4-HMAC-SHA256-EVENTS".to_owned()),
        &aws_sigv4::SigningParams {
          access_key: creds.access_key_id(),
          secret_key: creds.secret_access_key(),
          security_token: None,
          region: &region,
          service_name: "transcribe",
          date_time: Utc::now(),
          settings,
        },
      )
      .map_err(|e| anyhow!("failed to sign request: {}", e))?
      .into_parts();
    *last_sig_locked = sig;
    drop(last_sig_locked);
    
    for (key, value) in sig_headers {
      req.headers_mut().insert(key, value);
    }

    debug!("sending AWS streaming transcription request: {:?}", req);
    let res = client.request(req).await?;

    debug!("received response: {:?}", res);

    let status = res.status();
    let mut body = res.into_body();

    if !status.is_success() {
      let body = hyper::body::to_bytes(body).await?;
      error!("HTTP error: {:?}\nBody: {:?}", status, body);
      bail!("HTTP {}", status);
    }

    let mut buf = BytesMut::new();
    let mut frame_decoder = MessageFrameDecoder::new();
    while let Some(chunk) = body.try_next().await? {
      debug!("received chunk of {} bytes", chunk.len());
      buf.extend_from_slice(&chunk);
      if let DecodedFrame::Complete(message) = frame_decoder.decode_frame(&mut buf)? {
        let headers = message.headers();
        let payload = message.payload();
        let payload_str = std::str::from_utf8(&payload)?;

        let message_type = headers
          .iter()
          .find(|header| header.name().as_str() == ":message-type")
          .context("message has no message-type header")?
          .value()
          .as_string()
          .map_err(|_| anyhow!("malformed message-type header"))?
          .as_str();
        if message_type == "event" {
          let event_type = headers
            .iter()
            .find(|header| header.name().as_str() == ":event-type")
            .context("message has no event-type header")?
            .value()
            .as_string()
            .map_err(|_| anyhow!("malformed event-type header"))?
            .as_str();
          
          if event_type == "TranscriptEvent" {
            match serde_json::from_str::<AwsTranscriptEvent>(payload_str) {
              Ok(event) => {
                debug!("received transcript event: {:#?}", event);
                for result in event.transcript.results {
                  if result.alternatives.is_empty() {
                    continue;
                  }
                  let alternative = &result.alternatives[0];
                  transcription_tx.send(TranscriptionEvent::Speech {
                    timestamp: Utc::now(),
                    participant: participant.clone(),
                    is_interim: result.is_partial,
                    stability: if alternative.items.iter().all(|item| item.stable) { 1.0 } else { 0.9 },
                    language: "en-US".to_owned(),  // TODO
                    message_id: result.result_id,
                    transcript: vec![Transcript {
                      text: alternative.transcript.clone(),
                      confidence: alternative.items.iter().filter_map(|item| item.confidence).product(),
                    }],
                  }).await?;
                }
              },
              Err(e) => warn!("failed to deserialise event: {:?}", e),
            }
          }
          else {
            warn!("unhandled event type: {}", event_type);
          }
        }
        else if message_type == "exception" {
          error!("received exception from AWS: {}", payload_str);
        }
        else {
          warn!("unhandled message type: {}", message_type);
        }
      }
    }

    info!("reached end of transcription stream");
    abort_audio_stream.abort();

    Ok(())
  }
}

impl Transcriber for AwsTranscriber {
  fn new(region: Option<&str>, participant: &Participant, transcription_tx: mpsc::Sender<TranscriptionEvent>) -> Self {
    let (audio_tx, audio_rx) = mpsc::channel(1);
    info!("starting AWS transcriber");
    let region = region.map(|s| s.to_owned()).unwrap_or_default();
    let participant = participant.clone();
    let join_handle = tokio::spawn(async move {
      if let Err(e) = AwsTranscriber::transcribe(region, participant, audio_rx, transcription_tx).await {
        error!("AWS transcriber failed: {:?}", e);
      }
    });
    Self { audio_tx, join_handle }
  }

  fn make_element(&self) -> Result<Element> {
    debug!("creating appsink to stream samples from gstreamer pipeline");

    let bin = Bin::new(None);

    let opusparse = ElementFactory::make("opusparse", None)
      .context("failed to create opusparse element")?;
    bin.add(&opusparse)?;
    let sink_pad = opusparse
      .static_pad("sink")
      .context("opusparse has no sink pad")?;
    bin.add_pad(&GhostPad::with_target(Some("sink"), &sink_pad)?)?;

    let oggmux = ElementFactory::make("oggmux", None)
      .context("failed to create oggmux element")?;
    bin.add(&oggmux)?;
    opusparse.link(&oggmux)?;

    let appsink: AppSink = ElementFactory::make("appsink", None)
      .context("failed to create appsink element")?
      .dynamic_cast()
      .map_err(|_| anyhow!("appsink should be an AppSink"))?;
    appsink.set_property("drop", &true)?;
    appsink.set_property("max-buffers", &1u32)?;
    let audio_tx = self.audio_tx.clone();
    appsink.set_callbacks(
      AppSinkCallbacks::builder()
        .new_sample(move |appsink| {
          let sample = appsink.pull_sample()
            .map_err(|_| FlowError::Error)?;
          if let Some(buffer) = sample.buffer() {
            let mut raw_audio = BytesMut::with_capacity(buffer.size());
            raw_audio.resize(buffer.size(), 0);
            buffer.copy_to_slice(0, &mut raw_audio)
              .map_err(|_| {
                error!("failed to copy buffer");
                FlowError::Error
              })?;
            audio_tx.blocking_send(raw_audio.freeze())
              .map_err(|_| FlowError::Error)?;
          }
          Ok(FlowSuccess::Ok)
        })
        .build()
    );
    bin.add(&appsink)?;
    oggmux.link(&appsink)?;

    bin
      .dynamic_cast()
      .map_err(|_| anyhow!("bin should be an Element"))
  }
}
