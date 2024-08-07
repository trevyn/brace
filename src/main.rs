#![forbid(unsafe_code)]
#![allow(unused_imports, dead_code)]
#![feature(let_chains)]

use async_openai::types::Role::{self, *};
use async_openai::types::{
	ChatCompletionRequestMessage, ChatCompletionToolArgs, ChatCompletionToolType, FunctionObjectArgs,
};
use egui::text::LayoutJob;
use egui::*;
use futures::channel::mpsc::{self, Sender};
use once_cell::sync::Lazy;
use poll_promise::Promise;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use stream_cancel::{StreamExt as _, Trigger, Tripwire};
use turbosql::*;

mod audiofile;
mod self_update;
// mod session;

static TOKENIZER: Lazy<Mutex<tiktoken_rs::CoreBPE>> =
	Lazy::new(|| Mutex::new(tiktoken_rs::o200k_base().unwrap()));
static COMPLETION: Lazy<Mutex<String>> = Lazy::new(Default::default);
static COMPLETION_PROMPT: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::from("")));

#[derive(Clone)]
struct ChatMessage {
	role: Role,
	content: String,
	token_count: usize,
}

struct WheelWindow {
	open: bool,
	request_close: bool,
	messages: Vec<ChatMessage>,
}

impl Default for WheelWindow {
	fn default() -> Self {
		Self {
			open: true,
			request_close: false,
			messages: vec![ChatMessage { role: User, content: String::new(), token_count: 0 }],
		}
	}
}

static WHEEL_WINDOWS: Lazy<Mutex<Vec<WheelWindow>>> = Lazy::new(Default::default);

#[derive(Turbosql, Default)]
struct Setting {
	rowid: Option<i64>,
	key: String,
	value: String,
}

impl Setting {
	fn get(key: &str) -> Self {
		select!(Setting "WHERE key = " key)
			.unwrap_or(Setting { key: key.to_string(), ..Default::default() })
	}
	fn get_with_default(key: &str, default: &str) -> Self {
		select!(Setting "WHERE key = " key).unwrap_or(Setting {
			key: key.to_string(),
			value: default.to_string(),
			..Default::default()
		})
	}
	fn save(&self) {
		if self.rowid.is_some() {
			self.update().unwrap();
		} else {
			self.insert().unwrap();
		}
	}
}

#[derive(Turbosql, Default)]
struct Prompt {
	rowid: Option<i64>,
	time_ms: i64,
	prompt: String,
}

#[derive(Turbosql, Default)]
struct Document {
	rowid: Option<i64>,
	title: String,
	content: String,
	timestamp_ms: i64,
}

struct Resource {
	/// HTTP response
	response: ehttp::Response,

	text: Option<String>,

	/// If set, the response was an image.
	image: Option<Image<'static>>,
}

impl Resource {
	fn from_response(ctx: &Context, response: ehttp::Response) -> Self {
		let content_type = response.content_type().unwrap_or_default();
		if content_type.starts_with("image/") {
			ctx.include_bytes(response.url.clone(), response.bytes.clone());
			let image = Image::from_uri(response.url.clone());

			Self { response, text: None, image: Some(image) }
		} else {
			Self { response, text: None, image: None }
		}
	}
}

#[derive(Default, Deserialize, Serialize)]
pub struct App {
	url: String,
	line_selected: i64,
	title_text: String,
	question_text: String,
	answer_text: String,
	speaker_names: Vec<String>,
	system_text: String,
	prompt_text: String,
	completion_prompt: String,
	saved_version: String,

	#[serde(skip)]
	debounce_tx: Option<Sender<String>>,
	#[serde(skip)]
	gpt_3_trigger: Option<Trigger>,
	#[serde(skip)]
	trigger: Option<Trigger>,
	// #[serde(skip)]
	// sessions: Vec<session::Session>,
	#[serde(skip)]
	is_recording: bool,
	#[serde(skip)]
	promise: Option<Promise<ehttp::Result<Resource>>>,
	#[serde(skip)]
	tokenizer: Option<tiktoken_rs::CoreBPE>,
}

impl App {
	pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
		cc.egui_ctx.set_visuals(egui::style::Visuals::dark());

		egui_extras::install_image_loaders(&cc.egui_ctx);

		let (debounce_tx, mut _debounce_rx) = mpsc::channel(10);

		let s = Self {
			debounce_tx: Some(debounce_tx),
			// sessions: session::Session::calculate_sessions(),
			completion_prompt: COMPLETION_PROMPT.lock().unwrap().clone(),
			tokenizer: Some(tiktoken_rs::o200k_base().unwrap()),
			saved_version: select!(Option<Document> "ORDER BY timestamp_ms DESC LIMIT 1")
				.unwrap()
				.unwrap_or_default()
				.content,
			..Default::default()
		};

		let ctx_cloned = cc.egui_ctx.clone();

		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_millis(100));
			loop {
				interval.tick().await;
				ctx_cloned.request_repaint();
			}
		});

		// let ctx = cc.egui_ctx.clone();

		// Listen for events
		// tokio::spawn(async move {
		// 	let duration = Duration::from_millis(300);
		// 	let mut keys_pressed = false;
		// 	let mut string = String::new();
		// 	let mut _trigger = None;

		// 	loop {
		// 		match tokio::time::timeout(duration, debounce_rx.next()).await {
		// 			Ok(Some(s)) => {
		// 				// keyboard activity
		// 				_trigger = None;
		// 				COMPLETION.lock().unwrap().clear();
		// 				string = s;
		// 				keys_pressed = true;
		// 			}
		// 			Ok(None) => {
		// 				eprintln!("Debounce finished");
		// 				break;
		// 			}
		// 			Err(_) => {
		// 				if keys_pressed && !string.is_empty() {
		// 					// eprintln!("{:?} since keyboard activity: {}", duration, &string);
		// 					let (t, tripwire) = Tripwire::new();
		// 					_trigger = Some(t);
		// 					eprintln!("{}", string);
		// 					let string = format!("{} {}", COMPLETION_PROMPT.lock().unwrap(), string);
		// 					let ctx = ctx.clone();
		// 					tokio::spawn(async move {
		// 						COMPLETION.lock().unwrap().clear();
		// 						let ctx = ctx.clone();
		// 						run_openai_completion(tripwire, string, move |content| {
		// 							// eprint!("{}", content);
		// 							COMPLETION.lock().unwrap().push_str(&content);
		// 							ctx.request_repaint();
		// 						})
		// 						.await
		// 						.ok();
		// 					});
		// 					keys_pressed = false;
		// 				}
		// 			}
		// 		}
		// 	}
		// });

		// dbg!(&sessions);
		// let session = sessions.first().unwrap();
		// dbg!(session.duration_ms());
		// dbg!(session.samples().len());

		// Load previous app state (if any).
		// Note that you must enable the `persistence` feature for this to work.
		// if let Some(storage) = cc.storage {
		//     return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
		// }

		s
	}
}

trait MyThings {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool;
}

impl MyThings for Ui {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool {
		self
			.add(
				// vec2(400.0, 300.0),
				TextEdit::multiline(text)
					.desired_width(f32::INFINITY)
					// .desired_height(f32::INFINITY)
					.font(FontId::new(30.0, FontFamily::Proportional)),
			)
			.changed()
	}
}

impl eframe::App for App {
	fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
		let mut request_focus = None;
		let mut request_close = false;
		ctx.input(|i| {
			if i.key_pressed(Key::N) && i.modifiers.command {
				let mut wheel_windows = WHEEL_WINDOWS.lock().unwrap();
				let len = wheel_windows.len();
				wheel_windows.push(Default::default());
				request_focus = Some(len * 1000);
			}
			if i.key_pressed(Key::W) && i.modifiers.command {
				request_close = true;
			}
			if i.key_pressed(Key::S) && i.modifiers.command {
				let wheel_windows = WHEEL_WINDOWS.lock().unwrap();
				if let Some(window) = wheel_windows.get(0)
					&& let Some(message) = window.messages.get(0)
				{
					Document {
						rowid: None,
						title: "primary".into(),
						content: message.content.clone(),
						timestamp_ms: now_ms(),
					}
					.insert()
					.unwrap();
					self.saved_version = message.content.clone();
				}
			}
		});

		SidePanel::left("left_panel").show(ctx, |ui| {
			ui.label(option_env!("BUILD_ID").unwrap_or("DEV"));

			let wheel_windows = WHEEL_WINDOWS.lock().unwrap();
			if let Some(window) = wheel_windows.get(0)
				&& let Some(message) = window.messages.get(0)
			{
				if self.saved_version == message.content {
					ui.label("SAVED");
				}
			}

			let mut setting2 = Setting::get("openai_api_key");
			ui.label("openai api key:");
			ui
				.add(TextEdit::singleline(&mut setting2.value).desired_width(f32::INFINITY))
				.changed()
				.then(|| setting2.save());

			let mut setting3 = Setting::get_with_default("openai_model", "gpt-4o-mini");
			ui.label("openai model:");
			ui
				.add(TextEdit::singleline(&mut setting3.value).desired_width(f32::INFINITY))
				.changed()
				.then(|| setting3.save());

			ScrollArea::vertical().auto_shrink([false, false]).show(ui, |_ui| {
				// let size = [ui.available_width(), ui.spacing().interact_size.y.max(20.0)];
				// for card in cards {
				// 	let i = card.rowid.unwrap();
				// 	let label = SelectableLabel::new(i == self.line_selected, format!("{}: {}", i, card.title));
				// 	if ui.add_sized(size, label).clicked() {
				// 		self.line_selected = i;
				// 	}
				// }
			});
		});

		for (window_num, window) in WHEEL_WINDOWS.lock().unwrap().iter_mut().enumerate() {
			if window.request_close {
				window.open = false;
			}
			egui::Window::new(format!("wheel {}", window_num)).open(&mut window.open).show(ctx, |ui| {
				if request_close && Some(ui.layer_id()) == ui.ctx().top_layer_id() {
					window.request_close = true;
				}

				ScrollArea::vertical().show(ui, |ui| {
					if ui.button("copy all to clipboard").clicked() {
						let mut text = "\n".to_string();

						for entry in window.messages.iter() {
							text.push_str(&format!("[{}]: {}\n", entry.role, entry.content));
						}

						ui.output_mut(|o| o.copied_text = text);
					}
					let mut do_it = false;
					let mut do_it_j = 9999;
					let mut total_tokens = 0;
					for (j, entry) in window.messages.iter_mut().enumerate() {
						let id = Id::new(window_num * 1000 + j);
						let editor_has_focus = ui.ctx().memory(|m| m.has_focus(id));

						if editor_has_focus && ui.input_mut(|i| i.consume_key(Modifiers::default(), Key::Tab)) {
							entry.content.push_str(COMPLETION.lock().unwrap().as_str().split('\n').next().unwrap());
							COMPLETION.lock().unwrap().clear();
							if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
								let ccursor = egui::text::CCursor::new(entry.content.chars().count());
								state.cursor.set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
								state.store(ctx, id);
								// ui.ctx().memory().request_focus(text_edit_id); // give focus back to the `TextEdit`.
							}
						}
						if editor_has_focus
							&& ui
								.input_mut(|i| i.consume_key(Modifiers { command: true, ..Default::default() }, Key::Enter))
						{
							COMPLETION.lock().unwrap().clear();
							do_it = true;
							do_it_j = j;
						}

						ui.horizontal(|ui| {
							ui.radio_value(&mut entry.role, User, "user");
							ui.radio_value(&mut entry.role, System, "system");
							ui.radio_value(&mut entry.role, Assistant, "assistant");

							let mut layouter = |ui: &egui::Ui, string: &str, wrap_width: f32| {
								let mut job = LayoutJob::default();
								let completion = if editor_has_focus {
									let string = COMPLETION.lock().unwrap();
									string.split('\n').next().unwrap().to_owned()
								} else {
									String::new()
								};
								job.append(
									string,
									0.0,
									TextFormat {
										font_id: FontId::new(20.0, FontFamily::Monospace),
										color: if entry.role == Assistant {
											Color32::from_rgb(0xCD, 0xD8, 0xFF)
										} else {
											Color32::from_rgb(230, 230, 230)
										},
										..Default::default()
									},
								);
								job.append(
									&completion,
									0.0,
									TextFormat {
										font_id: FontId::new(20.0, FontFamily::Monospace),
										color: Color32::DARK_GRAY,
										..Default::default()
									},
								);
								job.wrap.max_width = wrap_width;
								ui.fonts(|f| f.layout_job(job))
							};

							if ui
								.add(
									TextEdit::multiline(&mut entry.content)
										.id(id)
										.lock_focus(true)
										// .font(FontId::new(20.0, FontFamily::Monospace))
										.desired_width(f32::INFINITY)
										.layouter(&mut layouter),
								)
								.changed()
							{
								entry.token_count =
									self.tokenizer.as_ref().unwrap().encode_with_special_tokens(&entry.content).len();
								// eprintln!("{}", entry.content);
								// let debounce_tx = self.debounce_tx.clone();
								// let entry_content = entry.content.clone();
								// tokio::spawn(async move {
								// 	debounce_tx.unwrap().send(entry_content).await.unwrap();
								// });
							};
							// if ui.button("remove").clicked() {
							// 	WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().0.remove(j);
							// }
						});
						ui.label(format!("{} tokens", entry.token_count));
						total_tokens += entry.token_count;
					}

					ui.label(
						egui::RichText::new(format!(
							"{} total tokens ({} cents) [command-enter to send]",
							total_tokens,
							((total_tokens * 15) as f64 / 1_000_000f64)
						))
						.color(Color32::WHITE),
					);

					let extra_space = ui.clip_rect().height() - 300.0;
					if extra_space > 5.0 {
						let response = ui.allocate_space(egui::Vec2::new(ui.available_width(), extra_space));
						if do_it {
							ui.scroll_to_rect(response.1, Some(Align::TOP))
						}
					}

					if let Some(id) = request_focus {
						ui.ctx().memory_mut(|m| m.request_focus(Id::new(id)))
					};

					if do_it {
						let ref mut messages = window.messages;
						messages.truncate(do_it_j + 1);
						Prompt { rowid: None, time_ms: now_ms(), prompt: messages.last().unwrap().content.clone() }
							.insert()
							.unwrap();
						let orig_messages = messages.clone();
						messages.push(ChatMessage { role: Assistant, content: String::new(), token_count: 0 });
						messages.push(ChatMessage { role: User, content: String::new(), token_count: 0 });
						ui.ctx().memory_mut(|m| m.request_focus(Id::new((window_num * 1000) + messages.len() - 1)));
						let id = messages.len() - 2;
						let ctx_cloned = ctx.clone();
						let (trigger, tripwire) = Tripwire::new();
						self.trigger = Some(trigger);
						tokio::spawn(async move {
							run_openai(Setting::get("openai_model").value, tripwire, orig_messages, move |content| {
								let mut wheel_windows = WHEEL_WINDOWS.lock().unwrap();
								let entry = wheel_windows.get_mut(window_num).unwrap().messages.get_mut(id).unwrap();
								entry.content.push_str(content);
								entry.token_count =
									TOKENIZER.lock().unwrap().encode_with_special_tokens(&entry.content).len();
								ctx_cloned.request_repaint();
							})
							.await
							.unwrap();
						});
					}
				});
			});
		}

		CentralPanel::default().show(ctx, |_ui| {});
	}
}

fn ui_url(ui: &mut Ui, _frame: &mut eframe::Frame, url: &mut String) -> bool {
	let mut trigger_fetch = false;

	ui.horizontal(|ui| {
		ui.label("URL:");
		trigger_fetch |= ui.add(TextEdit::singleline(url).desired_width(f32::INFINITY)).lost_focus();
	});

	ui.horizontal(|ui| {
		if ui.button("Random image").clicked() {
			let seed = ui.input(|i| i.time);
			let side = 640;
			*url = format!("https://picsum.photos/seed/{seed}/{side}");
			trigger_fetch = true;
		}
	});

	trigger_fetch
}

fn ui_resource(ui: &mut Ui, resource: &Resource) {
	let Resource { response, text, image } = resource;

	ui.monospace(format!("url:          {}", response.url));
	ui.monospace(format!("status:       {} ({})", response.status, response.status_text));
	ui.monospace(format!("content-type: {}", response.content_type().unwrap_or_default()));
	ui.monospace(format!("size:         {:.1} kB", response.bytes.len() as f32 / 1000.0));

	ui.separator();

	ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
		CollapsingHeader::new("Response headers").default_open(false).show(ui, |ui| {
			Grid::new("response_headers").spacing(vec2(ui.spacing().item_spacing.x * 2.0, 0.0)).show(
				ui,
				|ui| {
					for header in &response.headers {
						ui.label(&header.0);
						ui.label(&header.1);
						ui.end_row();
					}
				},
			)
		});

		ui.separator();

		if let Some(text) = &text {
			let tooltip = "Click to copy the response body";
			if ui.button("📋").on_hover_text(tooltip).clicked() {
				ui.ctx().copy_text(text.clone());
			}
			ui.separator();
		}

		if let Some(image) = image {
			ui.add(image.clone());
		} else if let Some(text) = &text {
			selectable_text(ui, text);
		} else {
			ui.monospace("[binary]");
		}
	});
}

fn selectable_text(ui: &mut Ui, mut text: &str) {
	ui.add(TextEdit::multiline(&mut text).desired_width(f32::INFINITY).font(TextStyle::Monospace));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

	eprintln!("database at {:?}", turbosql::db_path());

	self_update::self_update().await.ok();

	if let Some(document) = select!(Option<Document> "ORDER BY timestamp_ms DESC LIMIT 1")? {
		WHEEL_WINDOWS.lock().unwrap().push(WheelWindow {
			messages: vec![ChatMessage {
				role: User,
				content: document.content.clone(),
				token_count: TOKENIZER.lock().unwrap().encode_with_special_tokens(&document.content).len(),
			}],
			..Default::default()
		});
	}

	// Ok(())
	// let rt = tokio::runtime::Runtime::new().expect("Unable to create Runtime");

	// // Enter the runtime so that `tokio::spawn` is available immediately.
	// let _enter = rt.enter();

	// std::thread::spawn(move || rt.block_on(async {}));

	eframe::run_native(
		"brace",
		eframe::NativeOptions {
			viewport: egui::ViewportBuilder::default()
				.with_inner_size([400.0, 300.0])
				.with_min_inner_size([300.0, 220.0]),
			..Default::default()
		},
		Box::new(|cc| Ok(Box::new(App::new(cc)))),
	)?;

	Ok(())
}

pub(crate) async fn run_openai(
	model: impl AsRef<str>,
	tripwire: Tripwire,
	messages: Vec<ChatMessage>,
	callback: impl Fn(&String) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
	use async_openai::{types::CreateChatCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::with_config(
		async_openai::config::OpenAIConfig::new().with_api_key(Setting::get("openai_api_key").value),
	);

	let messages = messages
		.into_iter()
		.map(|m| match m.role {
			System => async_openai::types::ChatCompletionRequestSystemMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			User => async_openai::types::ChatCompletionRequestUserMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			Assistant => async_openai::types::ChatCompletionRequestAssistantMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			_ => panic!("invalid role"),
		})
		.collect::<Vec<ChatCompletionRequestMessage>>();

	// if !transcript.is_empty() {
	// 	messages.insert(
	// 		0,
	// 		async_openai::types::ChatCompletionRequestUserMessageArgs::default()
	// 			.content(transcript)
	// 			.build()
	// 			.unwrap()
	// 			.into(),
	// 	);
	// }

	// dbg!(&messages);

	let request = CreateChatCompletionRequestArgs::default()
		.model(model.as_ref().to_owned())
		.max_tokens(16384u16)
		.messages(messages)
		// .tools(vec![ChatCompletionToolArgs::default()
		// 	.r#type(ChatCompletionToolType::Function)
		// 	.function(
		// 		FunctionObjectArgs::default()
		// 			.name("get_current_weather")
		// 			.description("Get the current weather in a given location")
		// 			.parameters(json!({
		// 							"type": "object",
		// 							"properties": {
		// 											"location": {
		// 															"type": "string",
		// 															"description": "The city and state, e.g. San Francisco, CA",
		// 											},
		// 											"unit": { "type": "string", "enum": ["celsius", "fahrenheit"] },
		// 							},
		// 							"required": ["location"],
		// 			}))
		// 			.build()?,
		// 	)
		// 	.build()?])
		.build()?;

	let mut stream = client.chat().create_stream(request).await?.take_until_if(tripwire);

	while let Some(result) = stream.next().await {
		// dbg!(&result);
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|chat_choice| {
					if let Some(ref content) = chat_choice.delta.content {
						callback(content);
					}
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
	}

	Ok(())
}
