#![feature(lint_reasons)]
#![allow(clippy::type_complexity)]
#![warn(missing_docs)]
//! This crate provides logging functions and configuration for [Bevy](https://bevyengine.org)
//! apps, and automatically configures platform specific log handlers (i.e. WASM or Android).
//!
//! The macros provided for logging are reexported from [`tracing`](https://docs.rs/tracing),
//! and behave identically to it.
//!
//! By default, the [`LogPlugin`] from this crate is included in Bevy's `DefaultPlugins`
//! and the logging macros can be used out of the box, if used.
//!
//! For more fine-tuned control over logging behavior, set up the [`LogPlugin`] or
//! `DefaultPlugins` during app initialization.

#[cfg(feature = "trace")]
use std::panic;
use std::{fs::File, sync::Arc};

#[cfg(target_os = "android")]
mod android_tracing;

#[cfg(feature = "trace_tracy_memory")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

use bevy_app::{App, Plugin};
use bevy_utils::tracing::{warn, Level};
use tracing_log::LogTracer;
#[cfg(feature = "tracing-chrome")]
use tracing_subscriber::fmt::{format::DefaultFields, FormattedFields};
use tracing_subscriber::{prelude::*, registry::Registry, EnvFilter};

/// Adds logging to Apps. This plugin is part of the `DefaultPlugins`. Adding
/// this plugin will setup a collector appropriate to your target platform:
/// * Using [`tracing-subscriber`](https://crates.io/crates/tracing-subscriber) by default,
/// logging to `stdout`.
/// * Using [`android_log-sys`](https://crates.io/crates/android_log-sys) on Android,
/// logging to Android logs.
/// * Using [`tracing-wasm`](https://crates.io/crates/tracing-wasm) in WASM, logging
/// to the browser console.
///
/// You can configure this plugin.
/// ```no_run
/// # use bevy_app::{App, NoopPluginGroup as DefaultPlugins, PluginGroup};
/// # use bevy_log::LogPlugin;
/// # use bevy_utils::tracing::Level;
/// fn main() {
///     App::new()
///         .add_plugins(DefaultPlugins.set(LogPlugin {
///             level: Level::DEBUG,
///             filter: "wgpu=error,bevy_render=info,bevy_ecs=trace".to_string(),
///         }))
///         .run();
/// }
/// ```
///
/// Log level can also be changed using the `RUST_LOG` environment variable.
/// For example, using `RUST_LOG=wgpu=error,bevy_render=info,bevy_ecs=trace cargo run ..`
///
/// It has the same syntax as the field [`LogPlugin::filter`], see [`EnvFilter`].
/// If you define the `RUST_LOG` environment variable, the [`LogPlugin`] settings
/// will be ignored.
///
/// If you want to setup your own tracing collector, you should disable this
/// plugin from `DefaultPlugins`:
/// ```no_run
/// # use bevy_app::{App, NoopPluginGroup as DefaultPlugins, PluginGroup};
/// # use bevy_log::LogPlugin;
/// fn main() {
///     App::new()
///         .add_plugins(DefaultPlugins.build().disable::<LogPlugin>())
///         .run();
/// }
/// ```
///
/// # Panics
///
/// This plugin should not be added multiple times in the same process. This plugin
/// sets up global logging configuration for **all** Apps in a given process, and
/// rerunning the same initialization multiple times will lead to a panic.
pub struct LogFuPlugin {
    /// Filters logs using the [`EnvFilter`] format
    pub filter: String,

    /// Filters out logs that are "less than" the given level.
    /// This can be further filtered using the `filter` setting.
    pub level: Level,
    /// should we create a log file in app dir?
    pub log_too_file: bool,
}

impl Default for LogFuPlugin {
    fn default() -> Self {
        Self {
            filter: "wgpu=error,naga=warn".to_string(),
            level: Level::INFO,
            log_too_file: false,
        }
    }
}
impl Plugin for LogFuPlugin {
    //# cant move bits of this into separate functions
    //# because i don't understand the borrowing rules
    //# neither does rust-analyzer apparently
    #[allow(clippy::too_many_lines)]
    #[cfg_attr(not(feature = "tracing-chrome"), allow(unused_variables))]
    fn build(&self, app: &mut App) {
        #[cfg(feature = "trace")]
        {
            let old_handler = panic::take_hook();
            panic::set_hook(Box::new(move |infos| {
                println!("{}", tracing_error::SpanTrace::capture());
                old_handler(infos);
            }));
        }

        let finished_subscriber;
        let default_filter = { format!("{},{}", self.level, self.filter) };
        let filter_layer = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&default_filter))
            .unwrap();
        let subscriber = Registry::default().with(filter_layer);

        #[cfg(feature = "trace")]
        let subscriber = subscriber.with(tracing_error::ErrorLayer::default());

        #[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
        {
            #[cfg(feature = "tracing-chrome")]
            let chrome_layer = {
                let mut layer = tracing_chrome::ChromeLayerBuilder::new();
                if let Ok(path) = std::env::var("TRACE_CHROME") {
                    layer = layer.file(path);
                }
                let (chrome_layer, guard) = layer
                    .name_fn(Box::new(|event_or_span| match event_or_span {
                        tracing_chrome::EventOrSpan::Event(event) => event.metadata().name().into(),
                        tracing_chrome::EventOrSpan::Span(span) => {
                            if let Some(fields) =
                                span.extensions().get::<FormattedFields<DefaultFields>>()
                            {
                                format!("{}: {}", span.metadata().name(), fields.fields.as_str())
                            } else {
                                span.metadata().name().into()
                            }
                        }
                    }))
                    .build();
                app.world.insert_non_send_resource(guard);
                chrome_layer
            };

            #[cfg(feature = "tracing-tracy")]
            let tracy_layer = tracing_tracy::TracyLayer::new();

            let fmt_layer = tracing_subscriber::fmt::Layer::default().with_writer(std::io::stderr);

            // bevy_render::renderer logs a `tracy.frame_mark` event every frame
            // at Level::INFO. Formatted logs should omit it.
            #[cfg(feature = "tracing-tracy")]
            let fmt_layer =
                fmt_layer.with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
                    meta.fields().field("tracy.frame_mark").is_none()
                }));

            let subscriber = subscriber.with(fmt_layer);

            #[cfg(feature = "tracing-chrome")]
            let subscriber = subscriber.with(chrome_layer);
            #[cfg(feature = "tracing-tracy")]
            let subscriber = subscriber.with(tracy_layer);

            let file_log_layer = if self.log_too_file {
                // A layer that logs events to a file.
                #[cfg(not(any(
                    target_arch = "wasm32",
                    target_arch = "android",
                    target_arch = "ios"
                )))]
                {
                    let file = File::create("debug.log");
                    let file = match file {
                        Ok(file) => file,
                        Err(error) => panic!("Error: {error:?}"),
                    };
                    Some(
                        tracing_subscriber::fmt::layer()
                            .pretty()
                            .with_ansi(false)
                            .with_writer(Arc::new(file)),
                    )
                }
                #[cfg(any(target_arch = "wasm32", target_arch = "android", target_arch = "ios"))]
                {
                    None
                }
            } else {
                None
            };

            let subscriber = subscriber.with(file_log_layer);

            finished_subscriber = subscriber;
        }

        #[cfg(target_arch = "wasm32")]
        {
            console_error_panic_hook::set_once();
            finished_subscriber = subscriber.with(tracing_wasm::WASMLayer::new(
                tracing_wasm::WASMLayerConfig::default(),
            ));
        }

        #[cfg(target_os = "android")]
        {
            finished_subscriber = subscriber.with(android_tracing::AndroidLayer::default());
        }

        let logger_already_set = LogTracer::init().is_err();
        let subscriber_already_set =
            bevy_utils::tracing::subscriber::set_global_default(finished_subscriber).is_err();

        match (logger_already_set, subscriber_already_set) {
            (true, true) => warn!(
                "Could not set global logger and tracing subscriber as they are already set. Consider disabling LogPlugin."
            ),
            (true, _) => warn!("Could not set global logger as it is already set. Consider disabling LogPlugin."),
            (_, true) => warn!("Could not set global tracing subscriber as it is already set. Consider disabling LogPlugin."),
            _ => (),
        }
    }
}

// TODO: implement file logging with optional tracing layer
// have log too file be optional bool
// let stdout_log = tracing_subscriber::fmt::layer();

// let subscriber = subscriber.with(
//     stdout_log
//         // Combine the filtered `stdout_log` layer with the
//         // `debug_log` layer, producing a new `Layered` layer.
//         .and_then(debug_log),
// );
