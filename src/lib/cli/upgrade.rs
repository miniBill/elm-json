use super::{util, Kind};
use crate::{
    diff,
    package::retriever::Retriever,
    project::{self, Application, Project},
    semver,
    solver::Resolver,
};
use anyhow::{bail, Context, Result};
use clap::ArgMatches;
use colored::Colorize;

pub async fn run<'a>(matches: &ArgMatches<'a>, offline: bool) -> Result<()> {
    util::with_elm_json(
        matches,
        offline,
        upgrade_application,
        async move |_, _, _| bail!(Kind::NotSupported),
    )
    .await
}

async fn upgrade_application<'a>(
    matches: &ArgMatches<'a>,
    offline: bool,
    info: Application,
) -> Result<()> {
    let strictness = if matches.is_present("unsafe") {
        semver::Strictness::Unsafe
    } else {
        semver::Strictness::Safe
    };
    let elm_version = info.elm_version();

    let mut retriever: Retriever = Retriever::new(&elm_version.into(), offline)
        .await
        .context(Kind::Unknown)?;

    retriever.add_deps(&info.dependencies(&strictness));
    retriever.add_deps(&info.test_dependencies(&strictness));

    let res = Resolver::new(&mut retriever)
        .solve()
        .await
        .context(Kind::NoResolution)?;

    let direct_deps: Vec<_> = info.dependencies.direct.keys().cloned().collect();
    let deps = project::reconstruct(&direct_deps, &res);

    if deps.0 == info.dependencies && deps.1 == info.test_dependencies {
        println!("\n{}\n", util::format_header("PACKAGES UP TO DATE").green());
        println!("All your dependencies appear to be up to date!");
        return Ok(());
    }

    println!(
        "\n{}\n",
        util::format_header("PACKAGE UPGRADES FOUND").green()
    );

    diff::show(
        diff::Kind::Direct,
        &info.dependencies.direct,
        &deps.0.direct,
    );
    diff::show(
        diff::Kind::Indirect,
        &info.dependencies.indirect,
        &deps.0.indirect,
    );
    diff::show(
        diff::Kind::DirectTest,
        &info.test_dependencies.direct,
        &deps.1.direct,
    );
    diff::show(
        diff::Kind::IndirectTest,
        &info.test_dependencies.indirect,
        &deps.1.indirect,
    );

    let updated = Project::Application(info.with(deps.0, deps.1));
    if util::confirm("Should I make these changes?", matches)? {
        util::write_elm_json(&updated, matches)?;
        println!("Saved updated elm.json!");
    } else {
        println!("Aborting!");
    }

    Ok(())
}
