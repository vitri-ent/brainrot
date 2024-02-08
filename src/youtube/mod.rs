use std::{collections::VecDeque, sync::OnceLock};

use futures_util::{Stream, StreamExt};
use regex::Regex;
use reqwest::{
	header::{self, HeaderMap, HeaderValue},
	StatusCode
};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

mod signaler;
mod types;
mod util;
pub use self::signaler::SignalerChannel;
use self::{
	types::{Action, GetLiveChatBody, GetLiveChatResponse, MessageRun},
	util::{SimdJsonRequestBody, SimdJsonResponseBody}
};

const TANGO_LIVE_ENDPOINT: &str = "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat";
const TANGO_REPLAY_ENDPOINT: &str = "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat_replay";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:123.0) Gecko/20100101 Firefox/123.0";

#[derive(Debug, Error)]
pub enum YouTubeError {
	#[error("impossible regex error")]
	Regex(#[from] regex::Error),
	#[error("error when deserializing: {0}")]
	Deserialization(#[from] simd_json::Error),
	#[error("missing continuation contents")]
	MissingContinuationContents,
	#[error("reached end of continuation")]
	EndOfContinuation,
	#[error("request timed out")]
	TimedOut,
	#[error("request returned bad HTTP status: {0}")]
	BadStatus(StatusCode),
	#[error("request error: {0}")]
	GeneralRequest(reqwest::Error),
	#[error("{0} is not a live stream")]
	NotStream(String),
	#[error("Failed to match InnerTube API key")]
	NoInnerTubeKey,
	#[error("Chat continuation token could not be found.")]
	NoChatContinuation,
	#[error("Error parsing URL: {0}")]
	URLParseError(#[from] url::ParseError)
}

impl From<reqwest::Error> for YouTubeError {
	fn from(value: reqwest::Error) -> Self {
		if value.is_timeout() {
			YouTubeError::TimedOut
		} else if value.is_status() {
			YouTubeError::BadStatus(value.status().unwrap())
		} else {
			YouTubeError::GeneralRequest(value)
		}
	}
}

pub(crate) fn get_http_client() -> &'static reqwest::Client {
	static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
	HTTP_CLIENT.get_or_init(|| {
		let mut headers = HeaderMap::new();
		// Set our Accept-Language to en-US so we can properly match substrings
		headers.append(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.5"));
		headers.append(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
		headers.append(header::REFERER, HeaderValue::from_static("https://www.youtube.com/"));
		reqwest::Client::builder().default_headers(headers).build().unwrap()
	})
}

#[derive(Clone, Debug)]
pub struct RequestOptions {
	pub(crate) api_key: String,
	pub(crate) client_version: String,
	pub(crate) live_status: bool
}

pub async fn get_options_from_live_page(live_id: impl AsRef<str>) -> Result<(RequestOptions, String), YouTubeError> {
	let live_id = live_id.as_ref();
	let page_contents = get_http_client()
		.get(format!("https://www.youtube.com/watch?v={live_id}"))
		.send()
		.await?
		.text()
		.await?;

	let live_status: bool;
	let live_now_regex = Regex::new(r#"['"]isLiveNow['"]:\s*(true)"#)?;
	let not_replay_regex = Regex::new(r#"['"]isReplay['"]:\s*(true)"#)?;
	if live_now_regex.find(&page_contents).is_some() {
		live_status = true;
	} else if not_replay_regex.find(&page_contents).is_some() {
		live_status = false;
	} else {
		return Err(YouTubeError::NotStream(live_id.to_string()));
	}

	let api_key_regex = Regex::new(r#"['"]INNERTUBE_API_KEY['"]:\s*['"](.+?)['"]"#).unwrap();
	let api_key = match api_key_regex.captures(&page_contents).and_then(|captures| captures.get(1)) {
		Some(matched) => matched.as_str().to_string(),
		None => return Err(YouTubeError::NoInnerTubeKey)
	};

	let client_version_regex = Regex::new(r#"['"]clientVersion['"]:\s*['"]([\d.]+?)['"]"#).unwrap();
	let client_version = match client_version_regex.captures(&page_contents).and_then(|captures| captures.get(1)) {
		Some(matched) => matched.as_str().to_string(),
		None => "2.20230801.08.00".to_string()
	};

	let continuation_regex = if live_status {
		Regex::new(
			r#"Live chat['"],\s*['"]selected['"]:\s*(?:true|false),\s*['"]continuation['"]:\s*\{\s*['"]reloadContinuationData['"]:\s*\{['"]continuation['"]:\s*['"](.+?)['"]"#
		)?
	} else {
		Regex::new(
			r#"Top chat replay['"],\s*['"]selected['"]:\s*true,\s*['"]continuation['"]:\s*\{\s*['"]reloadContinuationData['"]:\s*\{['"]continuation['"]:\s*['"](.+?)['"]"#
		)?
	};
	let continuation = match continuation_regex.captures(&page_contents).and_then(|captures| captures.get(1)) {
		Some(matched) => matched.as_str().to_string(),
		None => return Err(YouTubeError::NoChatContinuation)
	};

	Ok((RequestOptions { api_key, client_version, live_status }, continuation))
}
pub struct Author {
	pub display_name: String,
	pub id: String,
	pub avatar: String
}

pub struct ChatMessage {
	pub runs: Vec<MessageRun>,
	pub is_super: bool,
	pub author: Author,
	pub timestamp: i64,
	pub time_delta: Option<i64>
}

pub struct YouTubeChatPageProcessor<'r> {
	actions: Mutex<VecDeque<Action>>,
	request_options: &'r RequestOptions,
	continuation_token: Option<String>,
	pub signaler_topic: Option<String>
}

unsafe impl<'r> Send for YouTubeChatPageProcessor<'r> {}

impl<'r> YouTubeChatPageProcessor<'r> {
	pub fn new(response: GetLiveChatResponse, request_options: &'r RequestOptions) -> Result<Self, YouTubeError> {
		let continuation_token = if request_options.live_status {
			let continuation = &response
				.continuation_contents
				.as_ref()
				.ok_or(YouTubeError::MissingContinuationContents)?
				.live_chat_continuation
				.continuations[0];
			continuation
				.invalidation_continuation_data
				.as_ref()
				.map(|x| x.continuation.to_owned())
				.or_else(|| continuation.timed_continuation_data.as_ref().map(|x| x.continuation.to_owned()))
		} else {
			response
				.continuation_contents
				.as_ref()
				.ok_or(YouTubeError::MissingContinuationContents)?
				.live_chat_continuation
				.continuations[0]
				.live_chat_replay_continuation_data
				.as_ref()
				.map(|x| x.continuation.to_owned())
		};
		let signaler_topic = if request_options.live_status {
			response.continuation_contents.as_ref().unwrap().live_chat_continuation.continuations[0]
				.invalidation_continuation_data
				.as_ref()
				.map(|c| c.invalidation_id.topic.to_owned())
		} else {
			None
		};
		Ok(Self {
			actions: Mutex::new(VecDeque::from(if request_options.live_status {
				response
					.continuation_contents
					.ok_or(YouTubeError::MissingContinuationContents)?
					.live_chat_continuation
					.actions
					.unwrap_or_default()
			} else {
				response
					.continuation_contents
					.ok_or(YouTubeError::MissingContinuationContents)?
					.live_chat_continuation
					.actions
					.ok_or(YouTubeError::EndOfContinuation)?
			})),
			request_options,
			continuation_token,
			signaler_topic
		})
	}

	async fn next_page(&self, continuation_token: &String) -> Result<Self, YouTubeError> {
		let page = fetch_yt_chat_page(self.request_options, continuation_token).await?;
		YouTubeChatPageProcessor::new(page, self.request_options)
	}

	pub async fn cont(&self) -> Option<Result<Self, YouTubeError>> {
		if let Some(continuation_token) = &self.continuation_token {
			Some(self.next_page(continuation_token).await)
		} else {
			None
		}
	}
}

impl<'r> Iterator for &YouTubeChatPageProcessor<'r> {
	type Item = ChatMessage;

	fn next(&mut self) -> Option<Self::Item> {
		let mut next_action = None;
		while next_action.is_none() {
			match self.actions.try_lock().unwrap().pop_front() {
				Some(action) => {
					if let Some(replay) = action.replay_chat_item_action {
						for action in replay.actions {
							if next_action.is_some() {
								break;
							}

							if let Some(add_chat_item_action) = action.add_chat_item_action {
								if let Some(text_message_renderer) = &add_chat_item_action.item.live_chat_text_message_renderer {
									if text_message_renderer.message.is_some() {
										next_action.replace((add_chat_item_action, Some(replay.video_offset_time_msec)));
									}
								} else if let Some(superchat_renderer) = &add_chat_item_action.item.live_chat_paid_message_renderer {
									if superchat_renderer.live_chat_text_message_renderer.message.is_some() {
										next_action.replace((add_chat_item_action, Some(replay.video_offset_time_msec)));
									}
								}
							}
						}
					} else if let Some(action) = action.add_chat_item_action {
						if let Some(text_message_renderer) = &action.item.live_chat_text_message_renderer {
							if text_message_renderer.message.is_some() {
								next_action.replace((action, None));
							}
						} else if let Some(superchat_renderer) = &action.item.live_chat_paid_message_renderer {
							if superchat_renderer.live_chat_text_message_renderer.message.is_some() {
								next_action.replace((action, None));
							}
						}
					}
				}
				None => return None
			}
		}

		let (next_action, time_delta) = next_action.unwrap();
		let is_super = next_action.item.live_chat_paid_message_renderer.is_some();
		let renderer = if let Some(renderer) = next_action.item.live_chat_text_message_renderer {
			renderer
		} else if let Some(renderer) = next_action.item.live_chat_paid_message_renderer {
			renderer.live_chat_text_message_renderer
		} else {
			unimplemented!()
		};

		Some(ChatMessage {
			runs: renderer.message.unwrap().runs,
			is_super,
			author: Author {
				display_name: renderer
					.message_renderer_base
					.author_name
					.map(|x| x.simple_text)
					.unwrap_or_else(|| renderer.message_renderer_base.author_external_channel_id.to_owned()),
				id: renderer.message_renderer_base.author_external_channel_id.to_owned(),
				avatar: renderer.message_renderer_base.author_photo.thumbnails[renderer.message_renderer_base.author_photo.thumbnails.len() - 1]
					.url
					.to_owned()
			},
			timestamp: renderer.message_renderer_base.timestamp_usec.timestamp_millis(),
			time_delta
		})
	}
}

pub async fn fetch_yt_chat_page(options: &RequestOptions, continuation: impl AsRef<str>) -> Result<GetLiveChatResponse, YouTubeError> {
	let body = GetLiveChatBody::new(continuation.as_ref(), &options.client_version, "WEB");
	println!("{}", simd_json::to_string(&body)?);
	let response: GetLiveChatResponse = get_http_client()
		.post(Url::parse_with_params(
			if options.live_status { TANGO_LIVE_ENDPOINT } else { TANGO_REPLAY_ENDPOINT },
			[("key", options.api_key.as_str()), ("prettyPrint", "false")]
		)?)
		.simd_json(&body)?
		.send()
		.await?
		.simd_json()
		.await?;
	println!(
		"{}",
		Url::parse_with_params(
			if options.live_status { TANGO_LIVE_ENDPOINT } else { TANGO_REPLAY_ENDPOINT },
			[("key", options.api_key.as_str()), ("prettyPrint", "false")]
		)?
	);
	Ok(response)
}

pub async fn stream(
	options: &RequestOptions,
	continuation: impl AsRef<str>
) -> Result<impl Stream<Item = Result<ChatMessage, YouTubeError>> + '_, YouTubeError> {
	let initial_chat = fetch_yt_chat_page(options, continuation).await?;
	let topic = initial_chat.continuation_contents.as_ref().unwrap().live_chat_continuation.continuations[0]
		.invalidation_continuation_data
		.as_ref()
		.unwrap()
		.invalidation_id
		.topic
		.to_owned();
	let subscriber = SignalerChannel::new(topic).await?;
	let (mut receiver, _handle) = subscriber.spawn_event_subscriber().await?;
	Ok(async_stream::try_stream! {
		let mut processor = YouTubeChatPageProcessor::new(initial_chat, options).unwrap();
		for msg in &processor {
			yield msg;
		}

		while receiver.recv().await.is_ok() {
			match processor.cont().await {
				Some(Ok(s)) => {
					processor = s;
					for msg in &processor {
						yield msg;
					}

					subscriber.refresh_topic(processor.signaler_topic.as_ref().unwrap()).await;
				}
				Some(Err(e)) => {
					eprintln!("{e:?}");
					break;
				}
				None => {
					eprintln!("none");
					break;
				}
			}
		}
	})
}
