// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    errors::{FromMessagesError, ParseTestListError, WriteTestListError},
    helpers::{dylib_path, dylib_path_envvar, write_test_name},
    list::{BinaryList, BinaryListState, OutputFormat, RustBuildMeta, Styles, TestListState},
    target_runner::{PlatformRunner, TargetRunner},
    test_filter::TestFilterBuilder,
};
use camino::{Utf8Path, Utf8PathBuf};
use duct::Expression;
use guppy::{
    graph::{PackageGraph, PackageMetadata},
    PackageId,
};
use nextest_metadata::{
    BuildPlatform, RustTestBinarySummary, RustTestCaseSummary, RustTestSuiteSummary,
    TestListSummary,
};
use once_cell::sync::OnceCell;
use owo_colors::OwoColorize;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io,
    io::Write,
    path::PathBuf,
};

/// A Rust test binary built by Cargo. This artifact hasn't been run yet so there's no information
/// about the tests within it.
///
/// Accepted as input to [`TestList::new`].
#[derive(Clone, Debug)]
pub struct RustTestArtifact<'g> {
    /// A unique identifier for this test artifact.
    pub binary_id: String,

    /// Metadata for the package this artifact is a part of. This is used to set the correct
    /// environment variables.
    pub package: PackageMetadata<'g>,

    /// The path to the binary artifact.
    pub binary_path: Utf8PathBuf,

    /// The unique binary name defined in `Cargo.toml` or inferred by the filename.
    pub binary_name: String,

    /// The working directory that this test should be executed in. If None, the current directory
    /// will not be changed.
    pub cwd: Utf8PathBuf,

    /// The platform for which this test artifact was built.
    pub build_platform: BuildPlatform,
}

impl<'g> RustTestArtifact<'g> {
    /// Constructs a list of test binaries from the list of built binaries.
    pub fn from_binary_list(
        graph: &'g PackageGraph,
        binary_list: BinaryList,
        path_mapper: &PathMapper,
        platform_filter: Option<BuildPlatform>,
    ) -> Result<Vec<Self>, FromMessagesError> {
        let mut binaries = vec![];

        for binary in binary_list.rust_binaries {
            if platform_filter.is_some() && platform_filter != Some(binary.build_platform) {
                continue;
            }

            // Look up the executable by package ID.
            let package_id = PackageId::new(binary.package_id);
            let package = graph
                .metadata(&package_id)
                .map_err(FromMessagesError::PackageGraph)?;

            // Tests are run in the directory containing Cargo.toml
            let cwd = package
                .manifest_path()
                .parent()
                .unwrap_or_else(|| {
                    panic!(
                        "manifest path {} doesn't have a parent",
                        package.manifest_path()
                    )
                })
                .to_path_buf();

            let binary_path = path_mapper.map_binary(binary.path);
            let cwd = path_mapper.map_cwd(cwd);

            binaries.push(RustTestArtifact {
                binary_id: binary.id,
                package,
                binary_path,
                binary_name: binary.name,
                cwd,
                build_platform: binary.build_platform,
            })
        }

        Ok(binaries)
    }
}

/// List of test instances, obtained by querying the [`RustTestArtifact`] instances generated by Cargo.
#[derive(Clone, Debug)]
pub struct TestList<'g> {
    test_count: usize,
    rust_build_meta: RustBuildMeta<TestListState>,
    rust_suites: BTreeMap<Utf8PathBuf, RustTestSuite<'g>>,
    updated_dylib_path: OsString,
    // Computed on first access.
    skip_count: OnceCell<usize>,
}

/// A suite of tests within a single Rust test binary.
///
/// This is a representation of [`nextest_metadata::RustTestSuiteSummary`] used internally by the runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustTestSuite<'g> {
    /// A unique identifier for this binary.
    pub binary_id: String,

    /// Package metadata.
    pub package: PackageMetadata<'g>,

    /// The unique binary name defined in `Cargo.toml` or inferred by the filename.
    pub binary_name: String,

    /// The working directory that this test binary will be executed in. If None, the current directory
    /// will not be changed.
    pub cwd: Utf8PathBuf,

    /// The platform the test suite is for (host or target).
    pub build_platform: BuildPlatform,

    /// Test case names and other information about them.
    pub testcases: BTreeMap<String, RustTestCaseSummary>,
}

/// A helper for path remapping.
///
/// This is useful when running tests in a different directory, or a different computer, from building them.
#[derive(Default)]
pub struct PathMapper {
    workspace: Option<(Utf8PathBuf, Utf8PathBuf)>,
    target_dir: Option<(Utf8PathBuf, Utf8PathBuf)>,
}

impl PathMapper {
    /// Constructs the path mapper.
    pub fn new(
        graph: &PackageGraph,
        workspace: Option<Utf8PathBuf>,
        orig_target_dir: &Utf8Path,
        target_dir: Option<Utf8PathBuf>,
    ) -> Self {
        Self {
            workspace: workspace.map(|w| (graph.workspace().root().to_owned(), w)),
            target_dir: target_dir.map(|d| (orig_target_dir.to_owned(), d)),
        }
    }

    /// Constructs a no-op path mapper.
    pub fn noop() -> Self {
        Self {
            workspace: None,
            target_dir: None,
        }
    }

    pub(super) fn new_target_dir(&self) -> Option<&Utf8Path> {
        self.target_dir.as_ref().map(|(_, new)| new.as_path())
    }

    fn map_cwd(&self, path: Utf8PathBuf) -> Utf8PathBuf {
        match &self.workspace {
            Some((from, to)) => match path.strip_prefix(from) {
                Ok(p) => to.join(p),
                Err(_) => path,
            },
            None => path,
        }
    }

    fn map_binary(&self, path: Utf8PathBuf) -> Utf8PathBuf {
        match &self.target_dir {
            Some((from, to)) => match path.strip_prefix(from) {
                Ok(p) => to.join(p),
                Err(_) => path,
            },
            None => path,
        }
    }
}

impl<'g> TestList<'g> {
    /// Creates a new test list by running the given command and applying the specified filter.
    pub fn new(
        test_artifacts: impl IntoIterator<Item = RustTestArtifact<'g>>,
        rust_build_meta: &RustBuildMeta<BinaryListState>,
        path_mapper: &PathMapper,
        filter: &TestFilterBuilder,
        runner: &TargetRunner,
    ) -> Result<Self, ParseTestListError> {
        let mut test_count = 0;
        let rust_build_meta = rust_build_meta.map_paths(path_mapper);
        let updated_dylib_path = Self::create_dylib_path(&rust_build_meta)?;

        let test_artifacts = test_artifacts
            .into_iter()
            .map(|test_binary| {
                let (non_ignored, ignored) = test_binary.exec(&updated_dylib_path, runner)?;
                let (bin, info) = Self::process_output(
                    test_binary,
                    filter,
                    non_ignored.as_str(),
                    ignored.as_str(),
                )?;
                test_count += info.testcases.len();
                Ok((bin, info))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self {
            rust_suites: test_artifacts,
            rust_build_meta,
            updated_dylib_path,
            test_count,
            skip_count: OnceCell::new(),
        })
    }

    /// Creates a new test list with the given binary names and outputs.
    pub fn new_with_outputs(
        test_bin_outputs: impl IntoIterator<
            Item = (RustTestArtifact<'g>, impl AsRef<str>, impl AsRef<str>),
        >,
        rust_build_meta: &RustBuildMeta<BinaryListState>,
        path_mapper: &PathMapper,
        filter: &TestFilterBuilder,
    ) -> Result<Self, ParseTestListError> {
        let mut test_count = 0;

        let test_artifacts = test_bin_outputs
            .into_iter()
            .map(|(test_binary, non_ignored, ignored)| {
                let (bin, info) = Self::process_output(
                    test_binary,
                    filter,
                    non_ignored.as_ref(),
                    ignored.as_ref(),
                )?;
                test_count += info.testcases.len();
                Ok((bin, info))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        let rust_build_meta = rust_build_meta.map_paths(path_mapper);
        let updated_dylib_path = Self::create_dylib_path(&rust_build_meta)?;

        Ok(Self {
            rust_suites: test_artifacts,
            rust_build_meta,
            updated_dylib_path,
            test_count,
            skip_count: OnceCell::new(),
        })
    }

    /// Returns the total number of tests across all binaries.
    pub fn test_count(&self) -> usize {
        self.test_count
    }

    /// Returns the Rust build-related metadata for this test list.
    pub fn rust_build_meta(&self) -> &RustBuildMeta<TestListState> {
        &self.rust_build_meta
    }

    /// Returns the total number of skipped tests.
    pub fn skip_count(&self) -> usize {
        *self.skip_count.get_or_init(|| {
            self.iter_tests()
                .filter(|instance| !instance.test_info.filter_match.is_match())
                .count()
        })
    }

    /// Returns the total number of tests that aren't skipped.
    ///
    /// It is always the case that `run_count + skip_count == test_count`.
    pub fn run_count(&self) -> usize {
        self.test_count - self.skip_count()
    }

    /// Returns the total number of binaries that contain tests.
    pub fn binary_count(&self) -> usize {
        self.rust_suites.len()
    }

    /// Returns the tests for a given binary, or `None` if the binary wasn't in the list.
    pub fn get(&self, test_bin: impl AsRef<Utf8Path>) -> Option<&RustTestSuite> {
        self.rust_suites.get(test_bin.as_ref())
    }

    /// Returns the updated dynamic library path used for tests.
    pub fn updated_dylib_path(&self) -> &OsStr {
        &self.updated_dylib_path
    }

    /// Constructs a serializble summary for this test list.
    pub fn to_summary(&self) -> TestListSummary {
        let rust_suites = self
            .rust_suites
            .iter()
            .map(|(binary_path, info)| {
                let platform = if info.package.is_proc_macro() {
                    BuildPlatform::Host
                } else {
                    BuildPlatform::Target
                };
                let testsuite = RustTestSuiteSummary {
                    package_name: info.package.name().to_owned(),
                    binary: RustTestBinarySummary {
                        binary_name: info.binary_name.clone(),
                        package_id: info.package.id().repr().to_owned(),
                        binary_path: binary_path.clone(),
                        binary_id: info.binary_id.clone(),
                        build_platform: platform,
                    },
                    cwd: info.cwd.clone(),
                    testcases: info.testcases.clone(),
                };
                (info.binary_id.clone(), testsuite)
            })
            .collect();
        let mut summary = TestListSummary::new(self.rust_build_meta.to_summary());
        summary.test_count = self.test_count;
        summary.rust_suites = rust_suites;
        summary
    }

    /// Outputs this list to the given writer.
    pub fn write(
        &self,
        output_format: OutputFormat,
        writer: impl Write,
        colorize: bool,
    ) -> Result<(), WriteTestListError> {
        match output_format {
            OutputFormat::Human { verbose } => self
                .write_human(writer, verbose, colorize)
                .map_err(WriteTestListError::Io),
            OutputFormat::Serializable(format) => format
                .to_writer(&self.to_summary(), writer)
                .map_err(WriteTestListError::Json),
        }
    }

    /// Iterates over all the test binaries.
    pub fn iter(&self) -> impl Iterator<Item = (&Utf8Path, &RustTestSuite)> + '_ {
        self.rust_suites
            .iter()
            .map(|(path, info)| (path.as_path(), info))
    }

    /// Iterates over the list of tests, returning the path and test name.
    pub fn iter_tests(&self) -> impl Iterator<Item = TestInstance<'_>> + '_ {
        self.rust_suites.iter().flat_map(|(test_bin, bin_info)| {
            bin_info.testcases.iter().map(move |(name, test_info)| {
                TestInstance::new(name, test_bin, bin_info, test_info)
            })
        })
    }

    /// Outputs this list as a string with the given format.
    pub fn to_string(&self, output_format: OutputFormat) -> Result<String, WriteTestListError> {
        // Ugh this sucks. String really should have an io::Write impl that errors on non-UTF8 text.
        let mut buf = Vec::with_capacity(1024);
        self.write(output_format, &mut buf, false)?;
        Ok(String::from_utf8(buf).expect("buffer is valid UTF-8"))
    }

    // ---
    // Helper methods
    // ---

    // Empty list for tests.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            test_count: 0,
            rust_build_meta: RustBuildMeta::empty(),
            updated_dylib_path: OsString::new(),
            rust_suites: BTreeMap::new(),
            skip_count: OnceCell::new(),
        }
    }

    pub(crate) fn create_dylib_path(
        rust_build_meta: &RustBuildMeta<TestListState>,
    ) -> Result<OsString, ParseTestListError> {
        let dylib_path = dylib_path();
        let new_paths = rust_build_meta.dylib_paths();

        let mut updated_dylib_path: Vec<PathBuf> =
            Vec::with_capacity(dylib_path.len() + new_paths.len());
        updated_dylib_path.extend(
            new_paths
                .iter()
                .map(|path| path.clone().into_std_path_buf()),
        );
        updated_dylib_path.extend(dylib_path);

        std::env::join_paths(updated_dylib_path)
            .map_err(move |error| ParseTestListError::dylib_join_paths(new_paths, error))
    }

    fn process_output(
        test_binary: RustTestArtifact<'g>,
        filter: &TestFilterBuilder,
        non_ignored: impl AsRef<str>,
        ignored: impl AsRef<str>,
    ) -> Result<(Utf8PathBuf, RustTestSuite<'g>), ParseTestListError> {
        let mut tests = BTreeMap::new();

        // Treat ignored and non-ignored as separate sets of single filters, so that partitioning
        // based on one doesn't affect the other.
        let mut non_ignored_filter = filter.build();
        for test_name in Self::parse(non_ignored.as_ref())? {
            tests.insert(
                test_name.into(),
                RustTestCaseSummary {
                    ignored: false,
                    filter_match: non_ignored_filter.filter_match(&test_binary, test_name, false),
                },
            );
        }

        let mut ignored_filter = filter.build();
        for test_name in Self::parse(ignored.as_ref())? {
            // Note that libtest prints out:
            // * just ignored tests if --ignored is passed in
            // * all tests, both ignored and non-ignored, if --ignored is not passed in
            // Adding ignored tests after non-ignored ones makes everything resolve correctly.
            tests.insert(
                test_name.into(),
                RustTestCaseSummary {
                    ignored: true,
                    filter_match: ignored_filter.filter_match(&test_binary, test_name, true),
                },
            );
        }

        let RustTestArtifact {
            binary_id,
            package,
            binary_path,
            binary_name,
            cwd,
            build_platform: platform,
        } = test_binary;

        Ok((
            binary_path,
            RustTestSuite {
                binary_id,
                package,
                binary_name,
                testcases: tests,
                cwd,
                build_platform: platform,
            },
        ))
    }

    /// Parses the output of --list --format terse and returns a sorted list.
    fn parse(list_output: &str) -> Result<Vec<&'_ str>, ParseTestListError> {
        let mut list = Self::parse_impl(list_output).collect::<Result<Vec<_>, _>>()?;
        list.sort_unstable();
        Ok(list)
    }

    fn parse_impl(
        list_output: &str,
    ) -> impl Iterator<Item = Result<&'_ str, ParseTestListError>> + '_ {
        // The output is in the form:
        // <test name>: test
        // <test name>: test
        // ...

        list_output.lines().filter_map(move |line| {
            if line.ends_with(": benchmark") {
                // These lines are produced by the default Rust benchmark harness (#[bench]).
                // Ignore them.
                return None;
            }

            let res = line.strip_suffix(": test").ok_or_else(|| {
                ParseTestListError::parse_line(
                    format!(
                        "line '{}' did not end with the string ': test' or ': benchmark'",
                        line
                    ),
                    list_output,
                )
            });
            Some(res)
        })
    }

    fn write_human(&self, mut writer: impl Write, verbose: bool, colorize: bool) -> io::Result<()> {
        let mut styles = Styles::default();
        if colorize {
            styles.colorize();
        }

        for (test_bin, info) in &self.rust_suites {
            writeln!(writer, "{}:", info.binary_id.style(styles.binary_id))?;
            if verbose {
                writeln!(writer, "  {} {}", "bin:".style(styles.field), test_bin)?;
                writeln!(writer, "  {} {}", "cwd:".style(styles.field), info.cwd)?;
                writeln!(
                    writer,
                    "  {} {}",
                    "build platform:".style(styles.field),
                    info.build_platform,
                )?;
            }

            let mut indented = indent_write::io::IndentWriter::new("    ", &mut writer);

            if info.testcases.is_empty() {
                writeln!(indented, "(no tests)")?;
            } else {
                for (name, info) in &info.testcases {
                    write_test_name(name, &styles, &mut indented)?;
                    if !info.filter_match.is_match() {
                        write!(indented, " (skipped)")?;
                    }
                    writeln!(indented)?;
                }
            }
        }
        Ok(())
    }
}

impl<'g> RustTestArtifact<'g> {
    /// Run this binary with and without --ignored and get the corresponding outputs.
    fn exec(
        &self,
        dylib_path: &OsStr,
        runner: &TargetRunner,
    ) -> Result<(String, String), ParseTestListError> {
        let platform_runner = runner.for_build_platform(self.build_platform);

        let non_ignored = self.exec_single(false, dylib_path, platform_runner)?;
        let ignored = self.exec_single(true, dylib_path, platform_runner)?;
        Ok((non_ignored, ignored))
    }

    fn exec_single(
        &self,
        ignored: bool,
        dylib_path: &OsStr,
        runner: Option<&PlatformRunner>,
    ) -> Result<String, ParseTestListError> {
        let mut argv = Vec::new();

        let program: std::ffi::OsString = if let Some(runner) = runner {
            argv.extend(runner.args());
            argv.push(self.binary_path.as_str());
            runner.binary().into()
        } else {
            use duct::IntoExecutablePath;
            self.binary_path.as_std_path().to_executable()
        };

        argv.extend(["--list", "--format", "terse"]);
        if ignored {
            argv.push("--ignored");
        }

        let cmd = make_test_expression(program, argv, &self.cwd, &self.package, dylib_path)
            .stdout_capture();

        cmd.read().map_err(|error| {
            ParseTestListError::command(
                format!(
                    "'{} --list --format terse{}'",
                    self.binary_path,
                    if ignored { " --ignored" } else { "" }
                ),
                error,
            )
        })
    }
}

/// Represents a single test with its associated binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TestInstance<'a> {
    /// The name of the test.
    pub name: &'a str,

    /// The test binary.
    pub binary: &'a Utf8Path,

    /// Information about the binary.
    pub bin_info: &'a RustTestSuite<'a>,

    /// Information about the test.
    pub test_info: &'a RustTestCaseSummary,
}

impl<'a> TestInstance<'a> {
    /// Creates a new `TestInstance`.
    pub(crate) fn new(
        name: &'a (impl AsRef<str> + ?Sized),
        binary: &'a (impl AsRef<Utf8Path> + ?Sized),
        bin_info: &'a RustTestSuite,
        test_info: &'a RustTestCaseSummary,
    ) -> Self {
        Self {
            name: name.as_ref(),
            binary: binary.as_ref(),
            bin_info,
            test_info,
        }
    }

    /// Creates the command expression for this test instance.
    pub(crate) fn make_expression(
        &self,
        test_list: &TestList<'_>,
        target_runner: &TargetRunner,
    ) -> Expression {
        let platform_runner = target_runner.for_build_platform(self.bin_info.build_platform);
        // TODO: non-rust tests

        let mut args = Vec::new();

        let program: std::ffi::OsString = match platform_runner {
            Some(runner) => {
                args.extend(runner.args());
                args.push(self.binary.as_str());
                runner.binary().into()
            }
            None => {
                use duct::IntoExecutablePath;
                self.binary.as_std_path().to_executable()
            }
        };

        args.extend(["--exact", self.name, "--nocapture"]);
        if self.test_info.ignored {
            args.push("--ignored");
        }

        make_test_expression(
            program,
            args,
            &self.bin_info.cwd,
            &self.bin_info.package,
            test_list.updated_dylib_path(),
        )
    }
}

/// Create a duct Expression for a test binary with the given arguments, using the specified [`PackageMetadata`].
pub(crate) fn make_test_expression(
    program: OsString,
    args: impl IntoIterator<Item = impl Into<OsString>>,
    cwd: &Utf8PathBuf,
    package: &PackageMetadata<'_>,
    dylib_path: &OsStr,
) -> duct::Expression {
    let cmd = duct::cmd(program, args)
        .dir(cwd)
        // This environment variable is set to indicate that tests are being run under nextest.
        .env("NEXTEST", "1")
        // This environment variable is set to indicate that each test is being run in its own process.
        .env("NEXTEST_EXECUTION_MODE", "process-per-test")
        // These environment variables are set at runtime by cargo test:
        // https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates
        .env(
            "CARGO_MANIFEST_DIR",
            // CARGO_MANIFEST_DIR is set to the *new* cwd after path mapping.
            cwd,
        )
        .env(
            "NEXTEST_ORIGINAL_CARGO_MANIFEST_DIR",
            // NEXTEST_ORIGINAL_CARGO_MANIFEST_DIR is set to the *old* cwd.
            package.manifest_path().parent().unwrap(),
        )
        .env("CARGO_PKG_VERSION", format!("{}", package.version()))
        .env(
            "CARGO_PKG_VERSION_MAJOR",
            format!("{}", package.version().major),
        )
        .env(
            "CARGO_PKG_VERSION_MINOR",
            format!("{}", package.version().minor),
        )
        .env(
            "CARGO_PKG_VERSION_PATCH",
            format!("{}", package.version().patch),
        )
        .env(
            "CARGO_PKG_VERSION_PRE",
            format!("{}", package.version().pre),
        )
        .env("CARGO_PKG_AUTHORS", package.authors().join(":"))
        .env("CARGO_PKG_NAME", package.name())
        .env(
            "CARGO_PKG_DESCRIPTION",
            package.description().unwrap_or_default(),
        )
        .env("CARGO_PKG_HOMEPAGE", package.homepage().unwrap_or_default())
        .env("CARGO_PKG_LICENSE", package.license().unwrap_or_default())
        .env(
            "CARGO_PKG_LICENSE_FILE",
            package.license_file().unwrap_or_else(|| "".as_ref()),
        )
        .env(
            "CARGO_PKG_REPOSITORY",
            package.repository().unwrap_or_default(),
        )
        .env(dylib_path_envvar(), dylib_path);

    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{list::SerializableFormat, test_filter::RunIgnored};
    use guppy::CargoMetadata;
    use indoc::indoc;
    use maplit::btreemap;
    use nextest_metadata::{FilterMatch, MismatchReason};
    use once_cell::sync::Lazy;
    use pretty_assertions::assert_eq;
    use std::iter;

    #[test]
    fn test_parse_test_list() {
        // Lines ending in ': benchmark' (output by the default Rust bencher) should be skipped.
        let non_ignored_output = indoc! {"
            tests::foo::test_bar: test
            tests::baz::test_quux: test
            benches::should_be_skipped: benchmark
        "};
        let ignored_output = indoc! {"
            benches::ignored_should_be_skipped: benchmark
            tests::ignored::test_bar: test
            tests::baz::test_ignored: test
        "};

        let test_filter = TestFilterBuilder::any(RunIgnored::Default);
        let fake_cwd: Utf8PathBuf = "/fake/cwd".into();
        let fake_binary_name = "fake-binary".to_owned();
        let fake_binary_id = "fake-package::fake-binary".to_owned();
        let test_binary = RustTestArtifact {
            binary_path: "/fake/binary".into(),
            cwd: fake_cwd.clone(),
            package: package_metadata(),
            binary_name: fake_binary_name.clone(),
            binary_id: fake_binary_id.clone(),
            build_platform: BuildPlatform::Target,
        };
        let rust_build_meta = RustBuildMeta::new("/fake");
        let test_list = TestList::new_with_outputs(
            iter::once((test_binary, &non_ignored_output, &ignored_output)),
            &rust_build_meta,
            &PathMapper::noop(),
            &test_filter,
        )
        .expect("valid output");
        assert_eq!(
            test_list.rust_suites,
            btreemap! {
                "/fake/binary".into() => RustTestSuite {
                    testcases: btreemap! {
                        "tests::foo::test_bar".to_owned() => RustTestCaseSummary {
                            ignored: false,
                            filter_match: FilterMatch::Matches,
                        },
                        "tests::baz::test_quux".to_owned() => RustTestCaseSummary {
                            ignored: false,
                            filter_match: FilterMatch::Matches,
                        },
                        "tests::ignored::test_bar".to_owned() => RustTestCaseSummary {
                            ignored: true,
                            filter_match: FilterMatch::Mismatch { reason: MismatchReason::Ignored },
                        },
                        "tests::baz::test_ignored".to_owned() => RustTestCaseSummary {
                            ignored: true,
                            filter_match: FilterMatch::Mismatch { reason: MismatchReason::Ignored },
                        },
                    },
                    cwd: fake_cwd,
                    build_platform: BuildPlatform::Target,
                    package: package_metadata(),
                    binary_name: fake_binary_name,
                    binary_id: fake_binary_id,
                }
            }
        );

        // Check that the expected outputs are valid.
        static EXPECTED_HUMAN: &str = indoc! {"
        fake-package::fake-binary:
            tests::baz::test_ignored (skipped)
            tests::baz::test_quux
            tests::foo::test_bar
            tests::ignored::test_bar (skipped)
        "};
        static EXPECTED_HUMAN_VERBOSE: &str = indoc! {"
            fake-package::fake-binary:
              bin: /fake/binary
              cwd: /fake/cwd
              build platform: target
                tests::baz::test_ignored (skipped)
                tests::baz::test_quux
                tests::foo::test_bar
                tests::ignored::test_bar (skipped)
        "};
        static EXPECTED_JSON_PRETTY: &str = indoc! {r#"
            {
              "rust-build-meta": {
                "target-directory": "/fake",
                "base-output-directories": [],
                "linked-paths": []
              },
              "test-count": 4,
              "rust-suites": {
                "fake-package::fake-binary": {
                  "package-name": "metadata-helper",
                  "binary-id": "fake-package::fake-binary",
                  "binary-name": "fake-binary",
                  "package-id": "metadata-helper 0.1.0 (path+file:///Users/fakeuser/local/testcrates/metadata/metadata-helper)",
                  "binary-path": "/fake/binary",
                  "build-platform": "target",
                  "cwd": "/fake/cwd",
                  "testcases": {
                    "tests::baz::test_ignored": {
                      "ignored": true,
                      "filter-match": {
                        "status": "mismatch",
                        "reason": "ignored"
                      }
                    },
                    "tests::baz::test_quux": {
                      "ignored": false,
                      "filter-match": {
                        "status": "matches"
                      }
                    },
                    "tests::foo::test_bar": {
                      "ignored": false,
                      "filter-match": {
                        "status": "matches"
                      }
                    },
                    "tests::ignored::test_bar": {
                      "ignored": true,
                      "filter-match": {
                        "status": "mismatch",
                        "reason": "ignored"
                      }
                    }
                  }
                }
              }
            }"#};

        assert_eq!(
            test_list
                .to_string(OutputFormat::Human { verbose: false })
                .expect("human succeeded"),
            EXPECTED_HUMAN
        );
        assert_eq!(
            test_list
                .to_string(OutputFormat::Human { verbose: true })
                .expect("human succeeded"),
            EXPECTED_HUMAN_VERBOSE
        );
        println!(
            "{}",
            test_list
                .to_string(OutputFormat::Serializable(SerializableFormat::JsonPretty))
                .expect("json-pretty succeeded")
        );
        assert_eq!(
            test_list
                .to_string(OutputFormat::Serializable(SerializableFormat::JsonPretty))
                .expect("json-pretty succeeded"),
            EXPECTED_JSON_PRETTY
        );
    }

    static PACKAGE_GRAPH_FIXTURE: Lazy<PackageGraph> = Lazy::new(|| {
        static FIXTURE_JSON: &str = include_str!("../../../fixtures/cargo-metadata.json");
        let metadata = CargoMetadata::parse_json(FIXTURE_JSON).expect("fixture is valid JSON");
        metadata
            .build_graph()
            .expect("fixture is valid PackageGraph")
    });

    static PACKAGE_METADATA_ID: &str = "metadata-helper 0.1.0 (path+file:///Users/fakeuser/local/testcrates/metadata/metadata-helper)";
    fn package_metadata() -> PackageMetadata<'static> {
        PACKAGE_GRAPH_FIXTURE
            .metadata(&PackageId::new(PACKAGE_METADATA_ID))
            .expect("package ID is valid")
    }
}
