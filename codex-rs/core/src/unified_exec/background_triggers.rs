use std::sync::Arc;

use regex_lite::Regex;
use tokio::time::Duration;
use tokio::time::Instant;

const OUTPUT_TAIL_MAX_CHARS: usize = 2_000;
const FLOAT_PATTERN: &str = r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?";

#[derive(Clone, Debug)]
pub(crate) struct BackgroundTriggerPolicy {
    triggers: Vec<BackgroundTriggerSpec>,
}

impl BackgroundTriggerPolicy {
    pub(crate) fn parse_declared(triggers: &[String]) -> Result<Option<Self>, String> {
        let mut parsed = Vec::new();
        for trigger in triggers {
            let trigger = trigger.trim();
            if trigger.is_empty()
                || trigger.eq_ignore_ascii_case("on_exit")
                || trigger.eq_ignore_ascii_case("failure_exit")
            {
                continue;
            }

            if let Some(pattern) = strip_trigger_prefix(trigger, "regex") {
                parsed.push(BackgroundTriggerSpec::Regex(RegexTriggerSpec::new(
                    trigger, pattern,
                )?));
                continue;
            }

            if let Some(duration) = strip_trigger_prefix(trigger, "no_output_for") {
                parsed.push(BackgroundTriggerSpec::NoOutputFor(NoOutputForTriggerSpec {
                    raw: trigger.to_string(),
                    duration: parse_duration(duration)?,
                }));
                continue;
            }

            if let Some(expression) = strip_trigger_prefix(trigger, "metric_threshold") {
                parsed.push(BackgroundTriggerSpec::MetricThreshold(
                    MetricThresholdTriggerSpec::new(trigger, expression)?,
                ));
                continue;
            }

            if let Some(expression) = strip_trigger_prefix(trigger, "metric_plateau")
                .or_else(|| strip_trigger_prefix(trigger, "plateau"))
            {
                parsed.push(BackgroundTriggerSpec::Plateau(PlateauTriggerSpec::new(
                    trigger, expression,
                )?));
                continue;
            }
        }

        if parsed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self { triggers: parsed }))
        }
    }
}

#[derive(Clone, Debug)]
enum BackgroundTriggerSpec {
    Regex(RegexTriggerSpec),
    NoOutputFor(NoOutputForTriggerSpec),
    MetricThreshold(MetricThresholdTriggerSpec),
    Plateau(PlateauTriggerSpec),
}

#[derive(Clone, Debug)]
struct RegexTriggerSpec {
    raw: String,
    regex: Arc<Regex>,
}

impl RegexTriggerSpec {
    fn new(raw: &str, pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(format!("`{raw}` is missing a regex pattern"));
        }
        let regex = Regex::new(pattern)
            .map_err(|err| format!("invalid regex in background trigger `{raw}`: {err}"))?;
        Ok(Self {
            raw: raw.to_string(),
            regex: Arc::new(regex),
        })
    }
}

#[derive(Clone, Debug)]
struct NoOutputForTriggerSpec {
    raw: String,
    duration: Duration,
}

#[derive(Clone, Debug)]
struct MetricThresholdTriggerSpec {
    raw: String,
    metric: String,
    comparator: MetricComparator,
    threshold: f64,
    metric_regex: Arc<Regex>,
}

impl MetricThresholdTriggerSpec {
    fn new(raw: &str, expression: &str) -> Result<Self, String> {
        let (metric, comparator, threshold) = parse_metric_threshold_expression(raw, expression)?;
        let metric_regex = metric_regex(&metric)?;
        Ok(Self {
            raw: raw.to_string(),
            metric,
            comparator,
            threshold,
            metric_regex,
        })
    }
}

#[derive(Clone, Debug)]
struct PlateauTriggerSpec {
    raw: String,
    metric: String,
    patience: usize,
    min_delta: f64,
    mode: PlateauMode,
    metric_regex: Arc<Regex>,
}

impl PlateauTriggerSpec {
    fn new(raw: &str, expression: &str) -> Result<Self, String> {
        let mut parts = expression.split_whitespace();
        let metric = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("`{raw}` is missing a metric name"))?
            .to_string();
        let mut patience = 10usize;
        let mut min_delta = 0.0f64;
        let mut mode = PlateauMode::Min;

        for part in parts {
            let Some((key, value)) = part.split_once('=') else {
                return Err(format!(
                    "invalid plateau option `{part}` in `{raw}`; expected key=value"
                ));
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "patience" => {
                    patience = value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| format!("invalid patience in `{raw}`"))?;
                    if patience == 0 {
                        return Err(format!("patience must be greater than zero in `{raw}`"));
                    }
                }
                "min_delta" => {
                    min_delta = value
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| format!("invalid min_delta in `{raw}`"))?;
                    if !min_delta.is_finite() || min_delta < 0.0 {
                        return Err(format!("min_delta must be non-negative in `{raw}`"));
                    }
                }
                "mode" => {
                    mode = match value.trim().to_ascii_lowercase().as_str() {
                        "min" => PlateauMode::Min,
                        "max" => PlateauMode::Max,
                        _ => return Err(format!("mode must be min or max in `{raw}`")),
                    };
                }
                _ => {
                    return Err(format!("unknown plateau option `{key}` in `{raw}`"));
                }
            }
        }

        let metric_regex = metric_regex(&metric)?;
        Ok(Self {
            raw: raw.to_string(),
            metric,
            patience,
            min_delta,
            mode,
            metric_regex,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricComparator {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl MetricComparator {
    fn evaluate(self, observed: f64, threshold: f64) -> bool {
        match self {
            Self::Lt => observed < threshold,
            Self::Le => observed <= threshold,
            Self::Gt => observed > threshold,
            Self::Ge => observed >= threshold,
            Self::Eq => (observed - threshold).abs() <= f64::EPSILON,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Eq => "==",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlateauMode {
    Min,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BackgroundTriggerFired {
    pub(crate) trigger: String,
    pub(crate) reason: String,
    pub(crate) output_tail: String,
}

pub(crate) struct BackgroundTriggerEvaluator {
    triggers: Vec<RuntimeTrigger>,
    last_output_at: Instant,
    output_tail: String,
}

impl BackgroundTriggerEvaluator {
    pub(crate) fn new(policy: BackgroundTriggerPolicy, now: Instant) -> Self {
        Self {
            triggers: policy
                .triggers
                .into_iter()
                .map(RuntimeTrigger::from_spec)
                .collect(),
            last_output_at: now,
            output_tail: String::new(),
        }
    }

    pub(crate) fn on_output(&mut self, text: &str, now: Instant) -> Vec<BackgroundTriggerFired> {
        self.last_output_at = now;
        let regex_scan_text = format!("{}{}", self.output_tail, text);
        push_tail(&mut self.output_tail, text);

        let mut fired = Vec::new();
        for trigger in &mut self.triggers {
            if let Some(event) = trigger.on_output(text, &regex_scan_text, &self.output_tail) {
                fired.push(event);
            }
        }
        fired
    }

    pub(crate) fn on_no_output_timeout(&mut self, now: Instant) -> Vec<BackgroundTriggerFired> {
        let mut fired = Vec::new();
        for trigger in &mut self.triggers {
            if let Some(event) =
                trigger.on_no_output_timeout(now, self.last_output_at, &self.output_tail)
            {
                fired.push(event);
            }
        }
        fired
    }

    pub(crate) fn next_no_output_deadline(&self) -> Option<Instant> {
        self.triggers
            .iter()
            .filter_map(|trigger| trigger.next_no_output_deadline(self.last_output_at))
            .min()
    }
}

enum RuntimeTrigger {
    Regex(RuntimeRegexTrigger),
    NoOutputFor(RuntimeNoOutputForTrigger),
    MetricThreshold(RuntimeMetricThresholdTrigger),
    Plateau(RuntimePlateauTrigger),
}

impl RuntimeTrigger {
    fn from_spec(spec: BackgroundTriggerSpec) -> Self {
        match spec {
            BackgroundTriggerSpec::Regex(spec) => {
                Self::Regex(RuntimeRegexTrigger { spec, fired: false })
            }
            BackgroundTriggerSpec::NoOutputFor(spec) => {
                Self::NoOutputFor(RuntimeNoOutputForTrigger { spec, fired: false })
            }
            BackgroundTriggerSpec::MetricThreshold(spec) => {
                Self::MetricThreshold(RuntimeMetricThresholdTrigger { spec, fired: false })
            }
            BackgroundTriggerSpec::Plateau(spec) => Self::Plateau(RuntimePlateauTrigger {
                spec,
                best: None,
                stale_observations: 0,
                fired: false,
            }),
        }
    }

    fn on_output(
        &mut self,
        text: &str,
        regex_scan_text: &str,
        output_tail: &str,
    ) -> Option<BackgroundTriggerFired> {
        match self {
            Self::Regex(trigger) => trigger.on_output(regex_scan_text, output_tail),
            Self::MetricThreshold(trigger) => trigger.on_output(text, output_tail),
            Self::Plateau(trigger) => trigger.on_output(text, output_tail),
            Self::NoOutputFor(_) => None,
        }
    }

    fn on_no_output_timeout(
        &mut self,
        now: Instant,
        last_output_at: Instant,
        output_tail: &str,
    ) -> Option<BackgroundTriggerFired> {
        match self {
            Self::NoOutputFor(trigger) => {
                trigger.on_no_output_timeout(now, last_output_at, output_tail)
            }
            _ => None,
        }
    }

    fn next_no_output_deadline(&self, last_output_at: Instant) -> Option<Instant> {
        match self {
            Self::NoOutputFor(trigger) if !trigger.fired => {
                Some(last_output_at + trigger.spec.duration)
            }
            _ => None,
        }
    }
}

struct RuntimeRegexTrigger {
    spec: RegexTriggerSpec,
    fired: bool,
}

impl RuntimeRegexTrigger {
    fn on_output(
        &mut self,
        regex_scan_text: &str,
        output_tail: &str,
    ) -> Option<BackgroundTriggerFired> {
        if self.fired || !self.spec.regex.is_match(regex_scan_text) {
            return None;
        }
        self.fired = true;
        Some(BackgroundTriggerFired {
            trigger: self.spec.raw.clone(),
            reason: "regex pattern matched process output".to_string(),
            output_tail: output_tail.to_string(),
        })
    }
}

struct RuntimeNoOutputForTrigger {
    spec: NoOutputForTriggerSpec,
    fired: bool,
}

impl RuntimeNoOutputForTrigger {
    fn on_no_output_timeout(
        &mut self,
        now: Instant,
        last_output_at: Instant,
        output_tail: &str,
    ) -> Option<BackgroundTriggerFired> {
        if self.fired || now < last_output_at + self.spec.duration {
            return None;
        }
        self.fired = true;
        Some(BackgroundTriggerFired {
            trigger: self.spec.raw.clone(),
            reason: format!("no output for {} seconds", self.spec.duration.as_secs_f64()),
            output_tail: output_tail.to_string(),
        })
    }
}

struct RuntimeMetricThresholdTrigger {
    spec: MetricThresholdTriggerSpec,
    fired: bool,
}

impl RuntimeMetricThresholdTrigger {
    fn on_output(&mut self, text: &str, output_tail: &str) -> Option<BackgroundTriggerFired> {
        if self.fired {
            return None;
        }
        for observed in extract_metric_values(&self.spec.metric_regex, text) {
            if self.spec.comparator.evaluate(observed, self.spec.threshold) {
                self.fired = true;
                return Some(BackgroundTriggerFired {
                    trigger: self.spec.raw.clone(),
                    reason: format!(
                        "{} {} {} matched with observed {}",
                        self.spec.metric,
                        self.spec.comparator.label(),
                        self.spec.threshold,
                        observed
                    ),
                    output_tail: output_tail.to_string(),
                });
            }
        }
        None
    }
}

struct RuntimePlateauTrigger {
    spec: PlateauTriggerSpec,
    best: Option<f64>,
    stale_observations: usize,
    fired: bool,
}

impl RuntimePlateauTrigger {
    fn on_output(&mut self, text: &str, output_tail: &str) -> Option<BackgroundTriggerFired> {
        if self.fired {
            return None;
        }
        for observed in extract_metric_values(&self.spec.metric_regex, text) {
            if self.is_improvement(observed) {
                self.best = Some(observed);
                self.stale_observations = 0;
                continue;
            }

            self.stale_observations = self.stale_observations.saturating_add(1);
            if self.stale_observations >= self.spec.patience {
                self.fired = true;
                let best = self.best.unwrap_or(observed);
                return Some(BackgroundTriggerFired {
                    trigger: self.spec.raw.clone(),
                    reason: format!(
                        "{} plateaued: best {}, latest {}, stale observations {}",
                        self.spec.metric, best, observed, self.stale_observations
                    ),
                    output_tail: output_tail.to_string(),
                });
            }
        }
        None
    }

    fn is_improvement(&self, observed: f64) -> bool {
        let Some(best) = self.best else {
            return true;
        };
        match self.spec.mode {
            PlateauMode::Min => observed <= best - self.spec.min_delta,
            PlateauMode::Max => observed >= best + self.spec.min_delta,
        }
    }
}

fn strip_trigger_prefix<'a>(trigger: &'a str, prefix: &str) -> Option<&'a str> {
    let trimmed = trigger.trim();
    if trimmed.len() < prefix.len() || !trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = trimmed[prefix.len()..].trim_start();
    if let Some(rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) {
        Some(rest.trim())
    } else {
        None
    }
}

fn parse_metric_threshold_expression(
    raw: &str,
    expression: &str,
) -> Result<(String, MetricComparator, f64), String> {
    for (op_text, comparator) in [
        ("<=", MetricComparator::Le),
        (">=", MetricComparator::Ge),
        ("==", MetricComparator::Eq),
        ("<", MetricComparator::Lt),
        (">", MetricComparator::Gt),
    ] {
        if let Some((metric, threshold)) = expression.split_once(op_text) {
            let metric = metric.trim();
            if metric.is_empty() {
                return Err(format!("`{raw}` is missing a metric name"));
            }
            let threshold = threshold
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("`{raw}` has an invalid threshold"))?;
            if !threshold.is_finite() {
                return Err(format!("`{raw}` threshold must be finite"));
            }
            return Ok((metric.to_string(), comparator, threshold));
        }
    }
    Err(format!(
        "`{raw}` must use one of <, <=, >, >=, == in the metric expression"
    ))
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("no_output_for trigger is missing a duration".to_string());
    }

    let split_at = value
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split_at);
    let amount = number
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid duration `{value}`"))?;
    if !amount.is_finite() || amount <= 0.0 {
        return Err(format!("duration `{value}` must be greater than zero"));
    }
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "ms" | "millisecond" | "milliseconds" => 0.001,
        other => return Err(format!("unsupported duration unit `{other}` in `{value}`")),
    };
    Ok(Duration::from_secs_f64(amount * multiplier))
}

fn metric_regex(metric: &str) -> Result<Arc<Regex>, String> {
    if metric.trim().is_empty() {
        return Err("metric name cannot be empty".to_string());
    }
    let escaped_metric = regex_lite::escape(metric.trim());
    let pattern = format!(r"(?i)\b{escaped_metric}\b\s*[:=]\s*({FLOAT_PATTERN})");
    Regex::new(&pattern)
        .map(Arc::new)
        .map_err(|err| format!("failed to compile metric parser for `{metric}`: {err}"))
}

fn extract_metric_values(regex: &Regex, text: &str) -> Vec<f64> {
    regex
        .captures_iter(text)
        .filter_map(|captures| captures.get(1))
        .filter_map(|value| value.as_str().parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .collect()
}

fn push_tail(tail: &mut String, text: &str) {
    tail.push_str(text);
    let char_count = tail.chars().count();
    if char_count <= OUTPUT_TAIL_MAX_CHARS {
        return;
    }
    let keep_from = char_count.saturating_sub(OUTPUT_TAIL_MAX_CHARS);
    *tail = tail.chars().skip(keep_from).collect();
}

#[cfg(test)]
#[path = "background_triggers_tests.rs"]
mod tests;
