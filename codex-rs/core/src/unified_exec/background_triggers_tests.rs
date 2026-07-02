use super::*;

fn policy(triggers: &[&str]) -> BackgroundTriggerPolicy {
    BackgroundTriggerPolicy::parse_declared(
        &triggers
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>(),
    )
    .expect("trigger policy should parse")
    .expect("policy should contain executable triggers")
}

#[test]
fn regex_trigger_fires_once_on_output() {
    let now = Instant::now();
    let mut evaluator =
        BackgroundTriggerEvaluator::new(policy(&["regex:CUDA out of memory|Traceback"]), now);

    let first = evaluator.on_output("epoch 1 ok\n", now + Duration::from_secs(1));
    assert!(first.is_empty());

    let second = evaluator.on_output(
        "RuntimeError: CUDA out of memory\n",
        now + Duration::from_secs(2),
    );
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].trigger, "regex:CUDA out of memory|Traceback");
    assert!(second[0].reason.contains("regex"));

    let third = evaluator.on_output(
        "RuntimeError: CUDA out of memory\n",
        now + Duration::from_secs(3),
    );
    assert!(third.is_empty());
}

#[test]
fn metric_threshold_fires_when_metric_crosses_threshold() {
    let now = Instant::now();
    let mut evaluator =
        BackgroundTriggerEvaluator::new(policy(&["metric_threshold:val_loss < 0.30"]), now);

    assert!(
        evaluator
            .on_output("epoch=1 val_loss=0.42\n", now)
            .is_empty()
    );
    let fired = evaluator.on_output("epoch=2 val_loss=0.298\n", now);

    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].trigger, "metric_threshold:val_loss < 0.30");
    assert!(fired[0].reason.contains("observed 0.298"));
}

#[test]
fn plateau_trigger_counts_stale_metric_observations() {
    let now = Instant::now();
    let mut evaluator = BackgroundTriggerEvaluator::new(
        policy(&["plateau:val_loss patience=3 min_delta=0.01"]),
        now,
    );

    assert!(
        evaluator
            .on_output("epoch=1 val_loss=0.50\n", now)
            .is_empty()
    );
    assert!(
        evaluator
            .on_output("epoch=2 val_loss=0.49\n", now)
            .is_empty()
    );
    assert!(
        evaluator
            .on_output("epoch=3 val_loss=0.488\n", now)
            .is_empty()
    );
    assert!(
        evaluator
            .on_output("epoch=4 val_loss=0.487\n", now)
            .is_empty()
    );
    let fired = evaluator.on_output("epoch=5 val_loss=0.489\n", now);

    assert_eq!(fired.len(), 1);
    assert_eq!(
        fired[0].trigger,
        "plateau:val_loss patience=3 min_delta=0.01"
    );
    assert!(fired[0].reason.contains("plateaued"));
}

#[test]
fn no_output_trigger_uses_last_output_time() {
    let now = Instant::now();
    let mut evaluator = BackgroundTriggerEvaluator::new(policy(&["no_output_for:10s"]), now);

    assert_eq!(
        evaluator.next_no_output_deadline(),
        Some(now + Duration::from_secs(10))
    );
    evaluator.on_output("still alive\n", now + Duration::from_secs(5));
    assert_eq!(
        evaluator.next_no_output_deadline(),
        Some(now + Duration::from_secs(15))
    );

    assert!(
        evaluator
            .on_no_output_timeout(now + Duration::from_secs(14))
            .is_empty()
    );
    let fired = evaluator.on_no_output_timeout(now + Duration::from_secs(15));

    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].trigger, "no_output_for:10s");
    assert!(fired[0].reason.contains("no output"));
    assert!(evaluator.next_no_output_deadline().is_none());
}

#[test]
fn invalid_structured_trigger_reports_parse_error() {
    let err = BackgroundTriggerPolicy::parse_declared(&[
        "metric_threshold:val_loss below 0.30".to_string()
    ])
    .expect_err("invalid threshold should fail");

    assert!(err.contains("must use one of"));
}
