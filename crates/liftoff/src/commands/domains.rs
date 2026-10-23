//! `domain add|ls|rm|set`.

use liftoff_api::{DomainCreate, DomainPatch, DomainView};
use reqwest::Method;

use super::{Ctx, parse, resolve, segment};
use crate::args::DomainCommand;
use crate::error::CliError;
use crate::{Sleeper, render};

/// Runs `domain …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: DomainCommand,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    match command {
        DomainCommand::Add {
            name,
            external,
            proxied,
        } => {
            let question = if external {
                "External host"
            } else {
                "Label under the base domain"
            };
            let name = ctx.text(name, "<label|host>", question)?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let project = resolve(&client, project).await?;
            let body = DomainCreate {
                project,
                name,
                external,
                proxied,
            };
            let reply = client
                .send_json(Method::POST, "/domains", &[], &body)
                .await?;
            ctx.show(&reply, |text| {
                Ok(render::domain(&parse::<DomainView>(text)?))
            })
        }
        DomainCommand::Ls => {
            ctx.finish()?;
            let reply = client.get("/domains", &[]).await?;
            ctx.show(&reply, |body| {
                Ok(render::domains(&parse::<Vec<DomainView>>(body)?))
            })
        }
        DomainCommand::Set { host, proxied } => {
            let host = ctx.text(host, "<host>", "Domain to change")?;
            let proxied = ctx.pick(
                proxied,
                "--proxied",
                "Proxied by the DNS module?",
                &[("proxied", true), ("not proxied", false)],
            )?;
            ctx.finish()?;
            let reply = client
                .send_json(
                    Method::PATCH,
                    &format!("/domains/{}", segment(&host)),
                    &[],
                    &DomainPatch { proxied },
                )
                .await?;
            ctx.show(&reply, |text| {
                Ok(render::domain(&parse::<DomainView>(text)?))
            })
        }
        DomainCommand::Rm { host } => {
            let host = ctx.text(host, "<host>", "Domain to remove")?;
            ctx.finish()?;
            let reply = client
                .delete(&format!("/domains/{}", segment(&host)), &[])
                .await?;
            ctx.ack(&reply, &format!("removed {host}"))
        }
    }
}
