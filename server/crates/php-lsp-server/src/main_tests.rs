use super::{
    worker_thread_stack_size_from_env_value, DEFAULT_WORKER_THREAD_STACK_SIZE,
    MIN_WORKER_THREAD_STACK_SIZE,
};

#[test]
fn worker_thread_stack_size_uses_default_for_missing_or_invalid_env() {
    assert_eq!(
        worker_thread_stack_size_from_env_value(None),
        DEFAULT_WORKER_THREAD_STACK_SIZE
    );
    assert_eq!(
        worker_thread_stack_size_from_env_value(Some("not-a-number".to_string())),
        DEFAULT_WORKER_THREAD_STACK_SIZE
    );
    assert_eq!(
        worker_thread_stack_size_from_env_value(Some(
            (MIN_WORKER_THREAD_STACK_SIZE - 1).to_string()
        )),
        DEFAULT_WORKER_THREAD_STACK_SIZE
    );
}

#[test]
fn worker_thread_stack_size_accepts_large_env_value() {
    let configured = MIN_WORKER_THREAD_STACK_SIZE * 2;
    assert_eq!(
        worker_thread_stack_size_from_env_value(Some(configured.to_string())),
        configured
    );
}
