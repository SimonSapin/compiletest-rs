// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "lib"]

#![feature(rustc_private)]
#![feature(test)]

#![deny(unused_imports)]

extern crate test;
extern crate rustc;
extern crate rustc_serialize;

#[macro_use]
extern crate log;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use common::{Config, Mode};
use common::{Pretty, DebugInfoGdb, DebugInfoLldb};
use test::TestPaths;
use std::borrow::ToOwned;
use rustc::session::config::host_triple;

use self::header::EarlyProps;

pub mod uidiff;
pub mod json;
pub mod procsrv;
pub mod util;
pub mod header;
pub mod runtest;
pub mod common;
pub mod errors;

pub fn default_config() -> Config {
    Config {
        compile_lib_path: PathBuf::from(""),
        run_lib_path: PathBuf::from(""),
        rustc_path: PathBuf::from("rustc"),
        rustdoc_path: PathBuf::from("rustdoc-path"),
        lldb_python: "python".to_owned(),
        docck_python: "docck-python".to_owned(),
        valgrind_path: None,
        force_valgrind: false,
        llvm_filecheck: None,
        src_base: PathBuf::from("tests/run-pass"),
        build_base: env::temp_dir(),
        stage_id: "stage-id".to_owned(),
        mode: Mode::RunPass,
        run_ignored: false,
        filter: None,
        filter_exact: false,
        logfile: None,
        runtool: None,
        host_rustcflags: None,
        target_rustcflags: None,
        target: host_triple().to_owned(),
        host: "(none)".to_owned(),
        gdb_version: None,
        lldb_version: None,
        llvm_version: None,
        android_cross_path: PathBuf::from("android-cross-path"),
        adb_path: "adb-path".to_owned(),
        adb_test_dir: "adb-test-dir/target".to_owned(),
        adb_device_status: false,
        lldb_python_dir: None,
        verbose: false,
        quiet: false,
        cc: "cc".to_string(),
        cxx: "cxx".to_string(),
        cflags: "cflags".to_string(),
        llvm_components: "llvm-components".to_string(),
        llvm_cxxflags: "llvm-cxxflags".to_string(),
        nodejs: None,
    }
}

pub fn run_tests(config: &Config) {
    if config.target.contains("android") {
        if let DebugInfoGdb = config.mode {
            println!("{} debug-info test uses tcp 5039 port.\
                     please reserve it", config.target);
        }

        // android debug-info test uses remote debugger
        // so, we test 1 thread at once.
        // also trying to isolate problems with adb_run_wrapper.sh ilooping
        env::set_var("RUST_TEST_THREADS","1");
    }

    if let DebugInfoLldb = config.mode {
        // Some older versions of LLDB seem to have problems with multiple
        // instances running in parallel, so only run one test task at a
        // time.
        env::set_var("RUST_TEST_TASKS", "1");
    }

    let opts = test_opts(config);
    let tests = make_tests(config);
    // sadly osx needs some file descriptor limits raised for running tests in
    // parallel (especially when we have lots and lots of child processes).
    // For context, see #8904
    // unsafe { raise_fd_limit::raise_fd_limit(); }
    // Prevent issue #21352 UAC blocking .exe containing 'patch' etc. on Windows
    // If #11207 is resolved (adding manifest to .exe) this becomes unnecessary
    env::set_var("__COMPAT_LAYER", "RunAsInvoker");
    let res = test::run_tests_console(&opts, tests.into_iter().collect());
    match res {
        Ok(true) => {}
        Ok(false) => panic!("Some tests failed"),
        Err(e) => {
            println!("I/O failure during tests: {:?}", e);
        }
    }
}

pub fn test_opts(config: &Config) -> test::TestOpts {
    test::TestOpts {
        filter: config.filter.clone(),
        filter_exact: config.filter_exact,
        run_ignored: config.run_ignored,
        quiet: config.quiet,
        logfile: config.logfile.clone(),
        run_tests: true,
        bench_benchmarks: true,
        nocapture: match env::var("RUST_TEST_NOCAPTURE") {
            Ok(val) => &val != "0",
            Err(_) => false
        },
        color: test::AutoColor,
        test_threads: None,
        skip: vec![],
        list: false,
    }
}

pub fn make_tests(config: &Config) -> Vec<test::TestDescAndFn> {
    debug!("making tests from {:?}",
           config.src_base.display());
    let mut tests = Vec::new();
    collect_tests_from_dir(config,
                           &config.src_base,
                           &config.src_base,
                           &PathBuf::new(),
                           &mut tests)
        .unwrap();
    tests
}

fn collect_tests_from_dir(config: &Config,
                          base: &Path,
                          dir: &Path,
                          relative_dir_path: &Path,
                          tests: &mut Vec<test::TestDescAndFn>)
                          -> io::Result<()> {
    // Ignore directories that contain a file
    // `compiletest-ignore-dir`.
    for file in try!(fs::read_dir(dir)) {
        let file = try!(file);
        let name = file.file_name();
        if name == *"compiletest-ignore-dir" {
            return Ok(());
        }
        if name == *"Makefile" && config.mode == Mode::RunMake {
            let paths = TestPaths {
                file: dir.to_path_buf(),
                base: base.to_path_buf(),
                relative_dir: relative_dir_path.parent().unwrap().to_path_buf(),
            };
            tests.push(make_test(config, &paths));
            return Ok(())
        }
    }

    // If we find a test foo/bar.rs, we have to build the
    // output directory `$build/foo` so we can write
    // `$build/foo/bar` into it. We do this *now* in this
    // sequential loop because otherwise, if we do it in the
    // tests themselves, they race for the privilege of
    // creating the directories and sometimes fail randomly.
    let build_dir = config.build_base.join(&relative_dir_path);
    fs::create_dir_all(&build_dir).unwrap();

    // Add each `.rs` file as a test, and recurse further on any
    // subdirectories we find, except for `aux` directories.
    let dirs = try!(fs::read_dir(dir));
    for file in dirs {
        let file = try!(file);
        let file_path = file.path();
        let file_name = file.file_name();
        if is_test(&file_name) {
            debug!("found test file: {:?}", file_path.display());
            // output directory `$build/foo` so we can write
            // `$build/foo/bar` into it. We do this *now* in this
            // sequential loop because otherwise, if we do it in the
            // tests themselves, they race for the privilege of
            // creating the directories and sometimes fail randomly.
            let build_dir = config.build_base.join(&relative_dir_path);
            fs::create_dir_all(&build_dir).unwrap();

            let paths = TestPaths {
                file: file_path,
                base: base.to_path_buf(),
                relative_dir: relative_dir_path.to_path_buf(),
            };
            tests.push(make_test(config, &paths))
        } else if file_path.is_dir() {
            let relative_file_path = relative_dir_path.join(file.file_name());
            if &file_name == "auxiliary" {
                // `aux` directories contain other crates used for
                // cross-crate tests. Don't search them for tests, but
                // do create a directory in the build dir for them,
                // since we will dump intermediate output in there
                // sometimes.
                let build_dir = config.build_base.join(&relative_file_path);
                fs::create_dir_all(&build_dir).unwrap();
            } else {
                debug!("found directory: {:?}", file_path.display());
                try!(collect_tests_from_dir(config,
                                       base,
                                       &file_path,
                                       &relative_file_path,
                                       tests));
            }
        } else {
            debug!("found other file/directory: {:?}", file_path.display());
        }
    }
    Ok(())
}

pub fn is_test(file_name: &OsString) -> bool {
    let file_name = file_name.to_str().unwrap();

    if !file_name.ends_with(".rs") {
        return false;
    }

    // `.`, `#`, and `~` are common temp-file prefixes.
    let invalid_prefixes = &[".", "#", "~"];
    !invalid_prefixes.iter().any(|p| file_name.starts_with(p))
}

pub fn make_test(config: &Config, testpaths: &TestPaths) -> test::TestDescAndFn {
    let early_props = EarlyProps::from_file(config, &testpaths.file);

    // The `should-fail` annotation doesn't apply to pretty tests,
    // since we run the pretty printer across all tests by default.
    // If desired, we could add a `should-fail-pretty` annotation.
    let should_panic = match config.mode {
        Pretty => test::ShouldPanic::No,
        _ => if early_props.should_fail {
            test::ShouldPanic::Yes
        } else {
            test::ShouldPanic::No
        }
    };

    test::TestDescAndFn {
        desc: test::TestDesc {
            name: make_test_name(config, testpaths),
            ignore: early_props.ignore,
            should_panic: should_panic,
        },
        testfn: make_test_closure(config, testpaths),
    }
}

pub fn make_test_name(config: &Config, testpaths: &TestPaths) -> test::TestName {
    // Convert a complete path to something like
    //
    //    run-pass/foo/bar/baz.rs
    let path =
        PathBuf::from(config.mode.to_string())
        .join(&testpaths.relative_dir)
        .join(&testpaths.file.file_name().unwrap());
    test::DynTestName(format!("[{}] {}", config.mode, path.display()))
}

pub fn make_test_closure(config: &Config, testpaths: &TestPaths) -> test::TestFn {
    let config = config.clone();
    let testpaths = testpaths.clone();
    test::DynTestFn(Box::new(move |()| {
        runtest::run(config, &testpaths)
    }))
}

#[allow(dead_code)]
fn extract_gdb_version(full_version_line: Option<String>) -> Option<String> {
    match full_version_line {
        Some(ref full_version_line)
          if !full_version_line.trim().is_empty() => {
            let full_version_line = full_version_line.trim();

            // used to be a regex "(^|[^0-9])([0-9]\.[0-9]+)"
            for (pos, c) in full_version_line.char_indices() {
                if !c.is_digit(10) {
                    continue
                }
                if pos + 2 >= full_version_line.len() {
                    continue
                }
                if full_version_line[pos + 1..].chars().next().unwrap() != '.' {
                    continue
                }
                if !full_version_line[pos + 2..].chars().next().unwrap().is_digit(10) {
                    continue
                }
                if pos > 0 && full_version_line[..pos].chars().next_back()
                                                      .unwrap().is_digit(10) {
                    continue
                }
                let mut end = pos + 3;
                while end < full_version_line.len() &&
                      full_version_line[end..].chars().next()
                                              .unwrap().is_digit(10) {
                    end += 1;
                }
                return Some(full_version_line[pos..end].to_owned());
            }
            println!("Could not extract GDB version from line '{}'",
                     full_version_line);
            None
        },
        _ => None
    }
}

#[allow(dead_code)]
fn extract_lldb_version(full_version_line: Option<String>) -> Option<String> {
    // Extract the major LLDB version from the given version string.
    // LLDB version strings are different for Apple and non-Apple platforms.
    // At the moment, this function only supports the Apple variant, which looks
    // like this:
    //
    // LLDB-179.5 (older versions)
    // lldb-300.2.51 (new versions)
    //
    // We are only interested in the major version number, so this function
    // will return `Some("179")` and `Some("300")` respectively.

    if let Some(ref full_version_line) = full_version_line {
        if !full_version_line.trim().is_empty() {
            let full_version_line = full_version_line.trim();

            for (pos, l) in full_version_line.char_indices() {
                if l != 'l' && l != 'L' { continue }
                if pos + 5 >= full_version_line.len() { continue }
                let l = full_version_line[pos + 1..].chars().next().unwrap();
                if l != 'l' && l != 'L' { continue }
                let d = full_version_line[pos + 2..].chars().next().unwrap();
                if d != 'd' && d != 'D' { continue }
                let b = full_version_line[pos + 3..].chars().next().unwrap();
                if b != 'b' && b != 'B' { continue }
                let dash = full_version_line[pos + 4..].chars().next().unwrap();
                if dash != '-' { continue }

                let vers = full_version_line[pos + 5..].chars().take_while(|c| {
                    c.is_digit(10)
                }).collect::<String>();
                if !vers.is_empty() { return Some(vers) }
            }
            println!("Could not extract LLDB version from line '{}'",
                     full_version_line);
        }
    }
    None
}

#[allow(dead_code)]
fn is_blacklisted_lldb_version(version: &str) -> bool {
    version == "350"
}
