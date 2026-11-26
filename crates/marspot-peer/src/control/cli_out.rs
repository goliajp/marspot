//! What `marspot-peer pane …` prints for a person, and its exit code.

use super::{Op, Response};
use crate::json::Value;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.str_at(k).unwrap_or("")
}

fn n(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::i64).map_or_else(|| "-".into(), |n| n.to_string())
}

/// The process exit code for an answer to `op`.
pub fn exit_code(op: &Op, resp: &Response) -> u8 {
    match (op, resp) {
        (_, Response::Err(_)) => 1,
        (Op::ProcStop(_), r) => super::cli_proc::exit_code(r),
        (Op::Run(_), Response::Ok(v)) => match v.get("exit_status").and_then(Value::i64) {
            Some(code) => code.clamp(0, 255) as u8,
            None => 1,
        },
        _ => 0,
    }
}

pub fn print(op: &Op, v: &Value) {
    match op {
        Op::List => list(v),
        Op::Open(_) => println!("opened {} (id {}, pos {})", s(v, "name"), n(v, "id"), n(v, "pos")),
        Op::Send(_) => {
            let echo = if v.get("echo_off").and_then(Value::bool) == Some(true) { ", echo off" } else { "" };
            println!("written to {} (pos {}{echo})", s(v, "pane"), n(v, "pos"));
        }
        Op::Run(_) | Op::Read(_) => {
            println!("{}", s(v, "text"));
            if let Some(m) = v.str_at("matched_line") {
                eprintln!("matched: {m}");
            }
            match v.str_at("reason") {
                Some("met") | None => {}
                Some(r) => eprintln!("{r} (pos {})", n(v, "pos")),
            }
        }
        Op::Audit { .. } => {
            for r in v.get("records").map(Value::arr).unwrap_or(&[]) {
                println!("{}", r.str().unwrap_or(""));
            }
        }
        Op::Resize { .. } | Op::Place { .. } | Op::Close { .. } => println!("ok"),
        Op::HelperStart(_) | Op::HelperList { .. } | Op::HelperStop(_) => super::cli_helper::print(op, v),
        Op::ProcStop(_) => super::cli_proc::print(v),
        Op::ResetClaim(_) | Op::ResetStatus => println!("{}", v.to_json()),
    }
}

fn list(v: &Value) {
    println!("{:>6}  {:<24}  {:<12}  {:<10}  {:<7}  directory", "id", "name", "state", "where", "agent");
    for p in v.get("panes").map(Value::arr).unwrap_or(&[]) {
        let mut state = s(p, "state").to_string();
        for flag in ["detached", "dormant", "shadow"] {
            if p.get(flag).and_then(Value::bool) == Some(true) {
                state = format!("{state},{flag}");
            }
        }
        let place = p.str_at("cell").map(String::from).unwrap_or_else(|| match p.get("parent").and_then(Value::i64) {
            Some(parent) => format!("under {parent}"),
            None => "-".into(),
        });
        let agent = p.str_at("agent").unwrap_or("-");
        println!("{:>6}  {:<24}  {:<12}  {:<10}  {:<7}  {}", n(p, "id"), s(p, "name"), state, place, agent, s(p, "cwd"));
    }
}
