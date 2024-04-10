#![warn(clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod app;
mod eprintln_driver;
pub(crate) mod example_id;
mod examples;
mod expression;
pub(crate) mod repl;

use clap::Parser;
use futures::{FutureExt, StreamExt};
use itertools::Itertools;

use crate::{
    app::{Inputs, Outputs},
    eprintln_driver::EprintlnDriver,
    expression::driver::ExpressionDriver,
    repl::driver::ReplDriver,
};

#[derive(Debug, clap::Parser)]
#[command(version, about)]
struct Cli {
    /// pattern (`glob` crate) of markdown filespaths
    sources: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let examples = examples::obtain(&cli.sources)?;
    if examples.is_empty() {
        anyhow::bail!("could not find any examples");
    }
    let (repl_driver, repl_events) = ReplDriver::new();
    let (expression_driver, expression_events) = ExpressionDriver::new();
    let (eprintln_driver, eprintln_events) = EprintlnDriver::new();

    let inputs = Inputs {
        examples,
        repl_events: repl_events.boxed_local(),
        expression_events: expression_events.boxed_local(),
        eprintln_events,
    };

    let outputs = app::app(inputs);

    let Outputs {
        repl_commands,
        expression_commands,
        done,
        execution_handle,
        eprintln_strings,
    } = outputs;

    let eprintln_task = eprintln_driver.init(eprintln_strings);
    let repl_task = repl_driver.init(repl_commands);
    let expression_task = expression_driver.init(expression_commands);

    tokio::select! {
        _ = execution_handle.fuse() => unreachable!(),
        _ = eprintln_task.fuse() => unreachable!(),
        _ = repl_task.fuse() => unreachable!(),
        _ = expression_task.fuse() => unreachable!(),
        done = done.fuse() => done,
    }
}

#[derive(Debug, Clone, derive_more::Display)]
#[display("{}", _0)]
struct Eprintln(String);

#[derive(Debug, derive_more::Deref)]
struct PtyLine(String);

impl std::str::FromStr for PtyLine {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.chars()
            .with_position()
            .try_for_each(|(position, character)| {
                use itertools::Position::*;
                match (position, character) {
                    (Last | Only, '\n') => Ok(()),
                    (Last | Only, _) => Err(anyhow::anyhow!("does not end with LF {s:?}")),
                    (_, '\n') => Err(anyhow::anyhow!("LF before end {s:?}")),
                    _ => Ok(()),
                }
            })?;

        Ok(Self(s.to_string()))
    }
}
