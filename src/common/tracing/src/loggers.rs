// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::fmt;
use std::io::BufWriter;
use std::time::Duration;
use std::time::SystemTime;

use fern::FormatCallback;
use opentelemetry::logs::AnyValue;
use opentelemetry::logs::Logger;
use opentelemetry::logs::LoggerProvider;
use opentelemetry::logs::Severity;
use opentelemetry_otlp::WithExportConfig;
use serde_json::Map;
use tracing_appender::non_blocking::NonBlocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;

/// Create a `BufWriter<NonBlocking>` for a rolling file logger.
///
/// `BufWriter` collects log segments into a whole before sending to underlying writer.
/// `NonBlocking` sends log to another thread to execute the write IO to avoid blocking the thread
/// that calls `log`.
///
/// Note that `NonBlocking` will discard logs if there are too many `io::Write::write(NonBlocking)`,
/// especially when `fern` sends log segments one by one to the `Writer`.
/// Therefore a `BufWriter` is used to reduce the number of `io::Write::write(NonBlocking)`.
pub(crate) fn new_file_log_writer(
    dir: &str,
    name: impl ToString,
    max_files: usize,
) -> (BufWriter<NonBlocking>, WorkerGuard) {
    let rolling = RollingFileAppender::builder()
        .rotation(Rotation::HOURLY)
        .filename_prefix(name.to_string())
        .max_log_files(max_files)
        .build(dir)
        .expect("failed to initialize rolling file appender");
    let (non_blocking, flush_guard) = tracing_appender::non_blocking(rolling);
    let buffered_non_blocking = BufWriter::with_capacity(64 * 1024 * 1024, non_blocking);

    (buffered_non_blocking, flush_guard)
}

pub(crate) struct MinitraceLogger;

impl log::Log for MinitraceLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let mut message = format!(
            "{} {:>5} {}{}",
            humantime::format_rfc3339_micros(SystemTime::now()),
            record.level(),
            record.args(),
            KvDisplay::new(record.key_values()),
        );
        if message.contains('\n') {
            // Align multi-line log messages with the first line after `level``.
            message = message.replace('\n', "\n                                  ");
        }
        minitrace::Event::add_to_local_parent(message, || []);
    }

    fn flush(&self) {}
}

pub(crate) struct OpenTelemetryLogger {
    logger: opentelemetry_sdk::logs::Logger,
    // keep provider alive
    provider: opentelemetry_sdk::logs::LoggerProvider,
}

impl OpenTelemetryLogger {
    pub(crate) fn new(
        name: impl ToString,
        endpoint: &str,
        labels: BTreeMap<String, String>,
    ) -> Self {
        let kvs = labels
            .into_iter()
            .map(|(k, v)| opentelemetry::KeyValue::new(k, v))
            .collect::<Vec<_>>();
        let export_config = opentelemetry_otlp::ExportConfig {
            endpoint: endpoint.to_string(),
            protocol: opentelemetry_otlp::Protocol::Grpc,
            timeout: Duration::from_secs(opentelemetry_otlp::OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT),
        };
        let exporter_builder: opentelemetry_otlp::LogExporterBuilder =
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_export_config(export_config)
                .into();
        let exporter = exporter_builder
            .build_log_exporter()
            .expect("build log exporter");
        let provider = opentelemetry_sdk::logs::LoggerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_config(
                opentelemetry_sdk::logs::Config::default()
                    .with_resource(opentelemetry_sdk::Resource::new(kvs)),
            )
            .build();
        let logger = provider.versioned_logger(name.to_string(), None, None, None);
        Self { logger, provider }
    }
}

impl log::Log for OpenTelemetryLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        // we handle level and target filter with fern
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let builder = opentelemetry::logs::LogRecord::builder()
            .with_observed_timestamp(SystemTime::now())
            .with_severity_number(map_severity_to_otel_severity(record.level()))
            .with_severity_text(record.level().as_str())
            .with_body(AnyValue::from(record.args().to_string()));
        self.logger.emit(builder.build())
    }

    fn flush(&self) {
        let result = self.provider.force_flush();
        for r in result {
            if let Err(e) = r {
                eprintln!("flush log failed: {}", e);
            }
        }
    }
}

pub fn formatter(
    format: &str,
) -> fn(out: FormatCallback, message: &fmt::Arguments, record: &log::Record) {
    match format {
        "text" => format_text_log,
        "json" => format_json_log,
        _ => unreachable!("file logging format {format} is not supported"),
    }
}

fn format_json_log(out: FormatCallback, message: &fmt::Arguments, record: &log::Record) {
    let mut fields = Map::new();
    fields.insert("message".to_string(), format!("{}", message).into());
    let mut visitor = KvCollector {
        fields: &mut fields,
    };
    record.key_values().visit(&mut visitor).ok();

    out.finish(format_args!(
        r#"{{"timestamp":"{}","level":"{}","fields":{}}}"#,
        humantime::format_rfc3339_micros(SystemTime::now()),
        record.level(),
        serde_json::to_string(&fields).unwrap_or_default(),
    ));

    struct KvCollector<'a> {
        fields: &'a mut Map<String, serde_json::Value>,
    }

    impl<'a, 'kvs> log::kv::Visitor<'kvs> for KvCollector<'a> {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'kvs>,
            value: log::kv::Value<'kvs>,
        ) -> Result<(), log::kv::Error> {
            self.fields
                .insert(key.as_str().to_string(), value.to_string().into());
            Ok(())
        }
    }
}

fn format_text_log(out: FormatCallback, message: &fmt::Arguments, record: &log::Record) {
    out.finish(format_args!(
        "{} {:>5} {}: {}:{} {}{}",
        humantime::format_rfc3339_micros(SystemTime::now()),
        record.level(),
        record.module_path().unwrap_or(""),
        record.file().unwrap_or(""),
        record.line().unwrap_or(0),
        message,
        KvDisplay::new(record.key_values()),
    ));
}

pub struct KvDisplay<'kvs> {
    kv: &'kvs dyn log::kv::Source,
}

impl<'kvs> KvDisplay<'kvs> {
    pub fn new(kv: &'kvs dyn log::kv::Source) -> Self {
        Self { kv }
    }
}

impl fmt::Display for KvDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut visitor = KvWriter { writer: f };
        self.kv.visit(&mut visitor).ok();
        Ok(())
    }
}

struct KvWriter<'a, 'kvs> {
    writer: &'kvs mut fmt::Formatter<'a>,
}

impl<'a, 'kvs> log::kv::Visitor<'kvs> for KvWriter<'a, 'kvs> {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        write!(self.writer, " {key}={value}")?;
        Ok(())
    }
}

fn map_severity_to_otel_severity(level: log::Level) -> Severity {
    match level {
        log::Level::Error => Severity::Error,
        log::Level::Warn => Severity::Warn,
        log::Level::Info => Severity::Info,
        log::Level::Debug => Severity::Debug,
        log::Level::Trace => Severity::Trace,
    }
}
