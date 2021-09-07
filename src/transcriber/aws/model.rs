use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AwsTranscriptEvent {
  pub(crate) transcript: AwsTranscript,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AwsTranscript {
  pub(crate) results: Vec<AwsTranscriptResult>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AwsTranscriptResult {
  pub(crate) alternatives: Vec<AwsTranscriptAlternative>,
  pub(crate) is_partial: bool,
  pub(crate) start_time: f64,
  pub(crate) end_time: f64,
  pub(crate) result_id: String,
  pub(crate) channel_id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AwsTranscriptAlternative {
  pub(crate) transcript: String,
  pub(crate) items: Vec<AwsTranscriptItem>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AwsTranscriptItem {
  pub(crate) r#type: String,
  pub(crate) content: String,
  pub(crate) start_time: f64,
  pub(crate) end_time: f64,
  pub(crate) stable: bool,
  pub(crate) vocabulary_filter_match: bool,
  pub(crate) confidence: Option<f64>,
  pub(crate) speaker: Option<String>,
}
