use std::{
    collections::BTreeSet,
    env::{current_dir, set_current_dir},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use lets_find_up::{find_up_with, FindUpKind, FindUpOptions};
use miette::{bail, ensure, miette, Context, Diagnostic, IntoDiagnostic, Report, SourceSpan};
use regex::Regex;

use crate::{
    fs::{copy_dir, create_dir_all, existing_file, remove_file, FileRoot},
    path::join_path,
    process::{which, EasyCommand},
};

mod fs;
mod path;
mod process;

/// Vendor WebGPU CTS tests from a local Git checkout of [our `gpuweb/cts` fork].
///
/// WPT tests are generated into `testing/web-platform/mozilla/tests/webgpu/`. If the set of tests
/// changes upstream, make sure that the generated output still matches up with test expectation
/// metadata in `testing/web-platform/mozilla/meta/webgpu/`.
///
/// [our `gpuweb/cts` fork]: https://github.com/mozilla/gpuweb-cts
#[derive(Debug, Parser)]
struct CliArgs {
    /// A path to the top-level directory of your WebGPU CTS checkout.
    cts_checkout_path: PathBuf,
    /// The maximum capacity for test variant chunks.
    ///
    /// This tools divides the large number of CTS tests generated by upstream and chunks them into
    /// multiple files. It's important to use a number that does not cause tests to time out in
    /// Taskcluster. The current default value has been empirically tested for only this criteria.
    /// Note that the way tests are divided may change in the future.
    ///
    /// If you intend to change the value long-term, change the default here.
    #[clap(long, default_value = "25")]
    chunk_size: NonZeroUsize,
}

fn main() -> ExitCode {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let args = CliArgs::parse();

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:?}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: CliArgs) -> miette::Result<()> {
    let CliArgs {
        chunk_size,
        cts_checkout_path,
    } = args;

    let orig_working_dir = current_dir().unwrap();

    let cts_dir = join_path(["dom", "webgpu", "tests", "cts"]);
    let cts_vendor_dir = join_path([&*cts_dir, "vendor".as_ref()]);
    let gecko_ckt = {
        let find_up_opts = || FindUpOptions {
            cwd: Path::new("."),
            kind: FindUpKind::Dir,
        };
        let find_up = |root_dir_name| {
            let err = || {
                miette!(
                    "failed to find a Mercurial repository ({root_dir_name:?}) in any of current \
                    working directory and its parent directories"
                )
            };
            find_up_with(root_dir_name, find_up_opts())
                .map_err(Report::msg)
                .wrap_err_with(err)
                .and_then(|loc_opt| loc_opt.ok_or_else(err))
                .map(|mut dir| {
                    dir.pop();
                    dir
                })
        };
        let gecko_source_root = find_up(".hg").or_else(|e| match find_up(".git") {
            Ok(path) => {
                log::debug!("{e:?}");
                Ok(path)
            }
            Err(e2) => {
                log::warn!("{e:?}");
                log::warn!("{e2:?}");
                bail!("failed to find a Gecko repository root")
            }
        })?;

        let root = FileRoot::new("gecko", &gecko_source_root)?;
        log::info!("detected Gecko repository root at {root}");

        ensure!(
            root.try_child(&orig_working_dir)
                .map_or(false, |c| c.relative_path() == cts_vendor_dir),
            "It is expected to run this tool from the root of its Cargo project, but this does \
            not appear to have been done. Bailing."
        );

        root
    };

    let cts_vendor_dir = gecko_ckt.child(orig_working_dir.parent().unwrap());

    let wpt_tests_dir = {
        let child = gecko_ckt.child(join_path(["testing", "web-platform", "mozilla", "tests"]));
        ensure!(
            child.is_dir(),
            "WPT tests dir ({child}) does not appear to exist"
        );
        child
    };

    let (cts_ckt_git_dir, cts_ckt) = {
        let failed_find_git_err = || {
            miette!(
                "failed to find a Git repository (`.git` directory) in the provided path and all \
                of its parent directories"
            )
        };
        let git_dir = find_up_with(
            ".git",
            FindUpOptions {
                cwd: &cts_checkout_path,
                kind: FindUpKind::Dir,
            },
        )
        .map_err(Report::msg)
        .wrap_err_with(failed_find_git_err)?
        .ok_or_else(failed_find_git_err)?;

        let ckt = FileRoot::new("cts", git_dir.parent().unwrap())?;
        log::debug!("detected CTS checkout root at {ckt}");
        (git_dir, ckt)
    };

    let git_bin = which("git", "Git binary")?;
    let npm_bin = which("npm", "NPM binary")?;

    // XXX: It'd be nice to expose separate operations for copying in source and generating WPT
    // cases from the vendored copy. Checks like these really only matter when updating source.
    let ensure_no_child = |p1: &FileRoot, p2| {
        ensure!(
            p1.try_child(p2).is_err(),
            "{p1} is a child path of {p2}, which is not supported"
        );
        Ok(())
    };
    ensure_no_child(&cts_ckt, &gecko_ckt)?;
    ensure_no_child(&gecko_ckt, &cts_ckt)?;

    log::info!("making a vendored copy of checked-in files from {cts_ckt}…",);
    gecko_ckt.regen_file(
        join_path([&*cts_dir, "checkout_commit.txt".as_ref()]),
        |checkout_commit_file| {
            let mut git_status_porcelain_cmd = EasyCommand::new(&git_bin, |cmd| {
                cmd.args(["status", "--porcelain"])
                    .envs([("GIT_DIR", &*cts_ckt_git_dir), ("GIT_WORK_TREE", &*cts_ckt)])
            });
            log::info!(
                "  …ensuring the working tree and index are clean with \
                    {git_status_porcelain_cmd}…"
            );
            let git_status_porcelain_output = git_status_porcelain_cmd.just_stdout_utf8()?;
            ensure!(
                git_status_porcelain_output.is_empty(),
                "expected a clean CTS working tree and index, but {}'s output was not empty; \
                for reference, it was:\n\n{}",
                git_status_porcelain_cmd,
                git_status_porcelain_output,
            );

            gecko_ckt.regen_dir(&cts_vendor_dir.join("checkout"), |vendored_ckt_dir| {
                log::info!("  …copying files tracked by Git to {vendored_ckt_dir}…");
                let files_to_vendor = {
                    let mut git_ls_files_cmd = EasyCommand::new(&git_bin, |cmd| {
                        cmd.arg("ls-files").env("GIT_DIR", &cts_ckt_git_dir)
                    });
                    log::debug!("  …getting files to vendor from {git_ls_files_cmd}…");
                    let output = git_ls_files_cmd.just_stdout_utf8()?;
                    let mut files = output
                        .split_terminator('\n')
                        .map(PathBuf::from)
                        .collect::<BTreeSet<_>>();
                    log::trace!("  …files from {git_ls_files_cmd}: {files:#?}");

                    log::trace!("  …validating that files from Git repo still exist…");
                    let files_not_found = files
                        .iter()
                        .filter(|p| !cts_ckt.child(p).exists())
                        .collect::<Vec<_>>();
                    ensure!(
                        files_not_found.is_empty(),
                        "the following files were returned by `git ls-files`, but do not \
                        exist on disk: {:#?}",
                        files_not_found,
                    );

                    log::trace!("  …stripping files we actually don't want to vendor…");
                    let files_to_actually_not_vendor = [
                        // There's no reason to bring this over, and lots of reasons to not bring in
                        // security-sensitive content unless we have to.
                        "deploy_key.enc",
                    ]
                    .map(Path::new);
                    log::trace!("    …files we don't want: {files_to_actually_not_vendor:?}");
                    for path in files_to_actually_not_vendor {
                        ensure!(
                            files.remove(path),
                            "failed to remove {} from list of files to vendor; does it still \
                            exist?",
                            cts_ckt.child(path)
                        );
                    }
                    files
                };

                log::debug!("  …now doing the copying…");
                for path in files_to_vendor {
                    let vendor_from_path = cts_ckt.child(&path);
                    let vendor_to_path = vendored_ckt_dir.child(&path);
                    if let Some(parent) = vendor_to_path.parent() {
                        create_dir_all(vendored_ckt_dir.child(parent))?;
                    }
                    log::trace!("    …copying {vendor_from_path} to {vendor_to_path}…");
                    fs::copy(&vendor_from_path, &vendor_to_path)?;
                }

                Ok(())
            })?;

            log::info!("  …writing commit ref pointed to by `HEAD` to {checkout_commit_file}…");
            let mut git_rev_parse_head_cmd = EasyCommand::new(&git_bin, |cmd| {
                cmd.args(["rev-parse", "HEAD"])
                    .env("GIT_DIR", &cts_ckt_git_dir)
            });
            log::trace!("    …getting output of {git_rev_parse_head_cmd}…");
            fs::write(
                checkout_commit_file,
                git_rev_parse_head_cmd.just_stdout_utf8()?,
            )
            .wrap_err_with(|| format!("failed to write HEAD ref to {checkout_commit_file}"))
        },
    )?;

    set_current_dir(&*cts_ckt)
        .into_diagnostic()
        .wrap_err("failed to change working directory to CTS checkout")?;
    log::debug!("changed CWD to {cts_ckt}");

    let mut npm_ci_cmd = EasyCommand::new(&npm_bin, |cmd| cmd.arg("ci"));
    log::info!(
        "ensuring a clean {} directory with {npm_ci_cmd}…",
        cts_ckt.child("node_modules"),
    );
    npm_ci_cmd.spawn()?;

    let out_wpt_dir = cts_ckt.regen_dir("out-wpt", |out_wpt_dir| {
        let mut npm_run_wpt_cmd = EasyCommand::new(&npm_bin, |cmd| cmd.args(["run", "wpt"]));
        log::info!("generating WPT test cases into {out_wpt_dir} with {npm_run_wpt_cmd}…");
        npm_run_wpt_cmd.spawn()
    })?;

    let cts_https_html_path = out_wpt_dir.child("cts.https.html");
    log::info!("refining the output of {cts_https_html_path} with `npm run gen_wpt_cts_html …`…");
    EasyCommand::new(&npm_bin, |cmd| {
        cmd.args(["run", "gen_wpt_cts_html"]).arg(existing_file(
            &cts_ckt.child("tools/gen_wpt_cfg_unchunked.json"),
        ))
    })
    .spawn()?;

    {
        let extra_cts_https_html_path = out_wpt_dir.child("cts-chunked2sec.https.html");
        log::info!("removing extraneous {extra_cts_https_html_path}…");
        remove_file(&*extra_cts_https_html_path)?;
    }

    log::info!("analyzing {cts_https_html_path}…");
    let cts_https_html_content = fs::read_to_string(&*cts_https_html_path)?;
    let cts_boilerplate;
    let cts_cases;
    {
        {
            let (boilerplate, cases_start) = {
                let cases_start_idx = cts_https_html_content
                    .find("<meta name=variant")
                    .ok_or_else(|| miette!("no test cases found; this is unexpected!"))?;
                cts_https_html_content.split_at(cases_start_idx)
            };

            cts_boilerplate = {
                if !boilerplate.is_empty() {
                    #[derive(Debug, Diagnostic, thiserror::Error)]
                    #[error("last character before test cases was not a newline; bug, or weird?")]
                    #[diagnostic(severity("warning"))]
                    struct Oops {
                        #[label(
                            "this character ({:?}) was expected to be a newline, so that the test \
                            spec. following it is on its own line",
                            source_code.chars().last().unwrap()
                        )]
                        span: SourceSpan,
                        #[source_code]
                        source_code: String,
                    }
                    ensure!(
                        boilerplate.ends_with('\n'),
                        Oops {
                            span: SourceSpan::from(0..boilerplate.len()),
                            source_code: cts_https_html_content,
                        }
                    );
                }

                // NOTE: Adding `_mozilla` is necessary because [that's how it's mounted][source].
                //
                // [source]: https://searchfox.org/mozilla-central/rev/cd2121e7d83af1b421c95e8c923db70e692dab5f/testing/web-platform/mozilla/README#1-4]
                log::info!(
                    "  …fixing `script` paths in WPT boilerplate so they work as Mozilla-private \
                    WPT tests…"
                );
                let expected_wpt_script_tag =
                    "<script type=module src=/webgpu/common/runtime/wpt.js></script>";
                ensure!(
                    boilerplate.contains(expected_wpt_script_tag),
                    "failed to find expected `script` tag for `wpt.js` \
                    ({expected_wpt_script_tag:?}); did something change upstream?",
                );
                let mut boilerplate = boilerplate.replacen(
                    expected_wpt_script_tag,
                    "<script type=module src=/_mozilla/webgpu/common/runtime/wpt.js></script>",
                    1,
                );

                // TODO: remove this?
                log::info!(
                    "  …adding long timeouts to WPT boilerplate to reduce timeout failures…"
                );
                let timeout_insert_idx = {
                    let meta_charset_utf8 = "\n<meta charset=utf-8>\n";
                    let meta_charset_utf8_idx =
                        boilerplate.find(meta_charset_utf8).ok_or_else(|| {
                            miette!(
                                "could not find {meta_charset_utf8:?} in document; did something \
                                change upstream?"
                            )
                        })?;
                    meta_charset_utf8_idx + meta_charset_utf8.len()
                };
                boilerplate.insert_str(
                    timeout_insert_idx,
                    concat!(
                        r#"<meta name="timeout" content="long">"#,
                        " <!-- TODO: narrow to only where it's needed, see ",
                        "https://bugzilla.mozilla.org/show_bug.cgi?id=1850537",
                        " -->\n"
                    ),
                );

                boilerplate
            };

            log::info!("  …parsing test variants in {cts_https_html_path}…");
            cts_cases = cases_start.split_terminator('\n').collect::<Vec<_>>();
            let mut parsing_failed = false;
            let meta_variant_regex =
                Regex::new(concat!("^", "<meta name=variant content='([^']*?)'>", "$")).unwrap();
            cts_cases.iter().for_each(|line| {
                if !meta_variant_regex.is_match(line) {
                    parsing_failed = true;
                    log::error!("line is not a test case: {line:?}");
                }
            });
            ensure!(
                !parsing_failed,
                "one or more test case lines failed to parse, fix it and try again"
            );
        };
        log::trace!("\"original\" HTML boilerplate:\n\n{cts_boilerplate}");

        ensure!(
            !cts_cases.is_empty(),
            "no test cases found; this is unexpected!"
        );
        log::info!("  …found {} test cases", cts_cases.len());
    }

    cts_ckt.regen_dir(out_wpt_dir.join("chunked"), |chunked_tests_dir| {
        // NOTE: We use an extremely simple chunking algorithm here. This was done in the name of
        // speed of initial implementation. However, this might cause a significant amount of churn
        // when tests get updated.
        let chunks = cts_cases.chunks(chunk_size.get()).zip(1u32..);
        log::info!(
            "re-distributing tests into {} chunks of {chunk_size}…",
            chunks.clone().count()
        );
        let mut failed_writing = false;
        for (chunk, chunk_idx) in chunks {
            // NOTE: Using `0`-padding here was considered, but it's probably not worth it. That
            // would be in conflict with stable file paths as the set of tests grows.
            let chunk_dir = chunked_tests_dir.child(chunk_idx.to_string());
            match create_dir_all(&chunk_dir) {
                Ok(()) => log::trace!("made directory {}", chunk_dir.display()),
                Err(e) => {
                    failed_writing = true;
                    log::error!("{e:#}");
                    continue;
                }
            }
            let chunk_file_path = chunk_dir.child("cts.https.html");
            let chunk_file_content = {
                let mut content = cts_boilerplate.as_bytes().to_vec();
                for line in chunk {
                    content.extend(line.as_bytes());
                    content.extend(b"\n");
                }
                content
            };
            match fs::write(&chunk_file_path, &chunk_file_content).wrap_err_with(|| {
                miette!("failed to write chunked output to path {chunk_file_path}")
            }) {
                Ok(()) => log::debug!("  …wrote {chunk_file_path}"),
                Err(e) => {
                    failed_writing = true;
                    log::error!("{e:#}");
                }
            }
        }
        ensure!(
            !failed_writing,
            "failed to write one or more chunked WPT test files; see above output for more details"
        );
        log::debug!("  …finished writing new chunked WPT test files!");

        log::info!("  …removing {cts_https_html_path}, now that it's been divided into chunks…");
        remove_file(&cts_https_html_path)?;

        Ok(())
    })?;

    gecko_ckt.regen_dir(wpt_tests_dir.join("webgpu"), |wpt_webgpu_tests_dir| {
        log::info!("copying contents of {out_wpt_dir} to {wpt_webgpu_tests_dir}…");
        copy_dir(&out_wpt_dir, wpt_webgpu_tests_dir)
    })?;

    log::info!("All done! Now get your CTS _ON_! :)");

    Ok(())
}
