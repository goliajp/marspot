//! Whose input a run types decides the frame it goes out on: the core
//! judges a program's, and delivers marspot's own as is.

use super::*;

#[test]
fn marspot_s_own_typing_is_sent_as_its_own_and_a_programs_as_a_programs() {
    let (state, env, host) = setup();
    let line = b"cd '/w' && claude --resume u --model 'claude-opus-5-5'\r".to_vec();
    let op = PtyOp::new("test.own").step(Step::send(line.clone())).step(Step::paste("carried"));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 1_000);
    assert_eq!(*state.sent.lock().unwrap(), [line]);
    assert_eq!(*state.pasted.lock().unwrap(), ["carried"]);
    assert!(state.programs.lock().unwrap().is_empty());

    let (state, env, host) = setup();
    let op = PtyOp::new("test.program").typed_for_program().step(Step::paste("/model")).step(Step::send(b"\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 1_000);
    assert!(state.sent.lock().unwrap().is_empty() && state.pasted.lock().unwrap().is_empty());
    assert_eq!(*state.programs.lock().unwrap(), [r#"Paste("/model")"#, r#"Bytes([13])"#]);
}
