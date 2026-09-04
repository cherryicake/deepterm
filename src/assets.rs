//! Embedded assets for DeepTerm
//!
//! Provides embedded SVG icons using rust_embed, plus the file-name /
//! file-extension icon mapping used by file trees and editors.
//!
//! File icons are from the Seti UI theme (MIT license), the default icon set
//! used by Visual Studio Code.

use anyhow::anyhow;
use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

/// Native implementation using RustEmbed
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Self::get(path)
            .map(|f| Some(f.data))
            .ok_or_else(|| anyhow!("asset not found: {}", path))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect())
    }
}

/// Icon names for embedded SVG assets
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Folder,
    File,
    Linux,
    Server,
    Screen,
    Macbook,
    Terminal,
    MagnifyingGlass,
    BroadcastInput,
    WindowClose,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    SettingsHub,
    Plus,
    PlusSquareOnSquare,
    SquareOnSquare,
    Copy,
    Check,
    Git,
    EllipsisCircleFill,
    Ellipsis,
    Host,
    KeyShield,
    ChevronDown,
    ChevronRight,
    ChevronUpDown,
    ChevronLeft,
    Eye,
    EyeOff,
    Loader,
    ProgressIndicator,
    // OS icons
    Macos,
    Ubuntu,
    Debian,
    Centos,
    Arch,
    Windows,
    LinuxMint,
    Fedora,
    Rhel,
    Opensuse,
    Manjaro,
    Workspace,
    Bell,
    Reveal,
    Sidebar,
    Theme,
    Font,
    PlusCircleFull,
    PersonBadgeKey,
    Clock,
    FolderOutline,
    FolderBadgePlus,
    XmarkCircleFill,
    // Git status icons
    SquareDot,   // Modified
    SquarePlus,  // Added
    SquareMinus, // Deleted
    Warning,     // Conflicted
    /// Dynamic file icon loaded by extension/name (e.g., language icons from Seti UI).
    FileIcon(&'static str),
}

impl Icon {
    /// Get the asset path for this icon
    pub fn path(&self) -> SharedString {
        // Use new_static to avoid allocation - these are compile-time constants
        match self {
            Icon::Folder => SharedString::new_static("icons/folder.fill.svg"),
            Icon::File => SharedString::new_static("icons/file.svg"),
            Icon::Linux => SharedString::new_static("icons/linux.svg"),
            Icon::Server => SharedString::new_static("icons/server.svg"),
            Icon::Screen => SharedString::new_static("icons/screen.svg"),
            Icon::Macbook => SharedString::new_static("icons/macbook.svg"),
            Icon::Terminal => SharedString::new_static("icons/terminal.svg"),
            Icon::MagnifyingGlass => SharedString::new_static("icons/magnifying_glass.svg"),
            Icon::BroadcastInput => SharedString::new_static("icons/broadcast_input.svg"),
            Icon::WindowClose => SharedString::new_static("icons/window-close.svg"),
            Icon::WindowMinimize => SharedString::new_static("icons/window-minimize.svg"),
            Icon::WindowMaximize => SharedString::new_static("icons/window-maximize.svg"),
            Icon::WindowRestore => SharedString::new_static("icons/window-restore.svg"),
            Icon::SettingsHub => SharedString::new_static("icons/settingshub.svg"),
            Icon::Plus => SharedString::new_static("icons/plus.svg"),
            Icon::PlusSquareOnSquare => SharedString::new_static("icons/plus.square.on.square.svg"),
            Icon::SquareOnSquare => SharedString::new_static("icons/square.on.square.svg"),
            Icon::Copy => SharedString::new_static("icons/copy.svg"),
            Icon::Check => SharedString::new_static("icons/check.svg"),
            Icon::Git => SharedString::new_static("icons/git.svg"),
            Icon::EllipsisCircleFill => SharedString::new_static("icons/ellipsis.circle.fill.svg"),
            Icon::Ellipsis => SharedString::new_static("icons/ellipsis.svg"),
            Icon::Host => SharedString::new_static("icons/host.svg"),
            Icon::KeyShield => SharedString::new_static("icons/key_shield.svg"),
            Icon::ChevronDown => SharedString::new_static("icons/chevron.down.svg"),
            Icon::ChevronRight => SharedString::new_static("icons/chevron.right.svg"),
            Icon::ChevronUpDown => SharedString::new_static("icons/chevron.up.chevron.down.svg"),
            Icon::ChevronLeft => SharedString::new_static("icons/chevron.left.svg"),
            Icon::Eye => SharedString::new_static("icons/eye.svg"),
            Icon::EyeOff => SharedString::new_static("icons/eye_off.svg"),
            Icon::Loader => SharedString::new_static("icons/loader.svg"),
            Icon::ProgressIndicator => SharedString::new_static("icons/progress.indicator.svg"),
            Icon::Macos => SharedString::new_static("icons/macos.svg"),
            Icon::Ubuntu => SharedString::new_static("icons/ubuntu.svg"),
            Icon::Debian => SharedString::new_static("icons/debian.svg"),
            Icon::Centos => SharedString::new_static("icons/centos.svg"),
            Icon::Arch => SharedString::new_static("icons/arch.svg"),
            Icon::Windows => SharedString::new_static("icons/windows.svg"),
            Icon::LinuxMint => SharedString::new_static("icons/linuxmint.svg"),
            Icon::Fedora => SharedString::new_static("icons/fedora.svg"),
            Icon::Rhel => SharedString::new_static("icons/rhel.svg"),
            Icon::Opensuse => SharedString::new_static("icons/opensuse.svg"),
            Icon::Manjaro => SharedString::new_static("icons/manjaro.svg"),
            Icon::Workspace => SharedString::new_static("icons/workspace.svg"),
            Icon::Bell => SharedString::new_static("icons/bell.svg"),
            Icon::Reveal => SharedString::new_static("icons/reveal.svg"),
            Icon::Sidebar => SharedString::new_static("icons/sidebar.svg"),
            Icon::Theme => SharedString::new_static("icons/theme.svg"),
            Icon::Font => SharedString::new_static("icons/font.svg"),
            Icon::PlusCircleFull => SharedString::new_static("icons/plus.circle.full.svg"),
            Icon::PersonBadgeKey => SharedString::new_static("icons/person.badge.key.fill.svg"),
            Icon::Clock => SharedString::new_static("icons/clock.svg"),
            Icon::FolderOutline => SharedString::new_static("icons/folder.svg"),
            Icon::FolderBadgePlus => SharedString::new_static("icons/folder.badge.plus.svg"),
            Icon::XmarkCircleFill => SharedString::new_static("icons/xmark.circle.fill.svg"),
            Icon::SquareDot => SharedString::new_static("icons/square_dot.svg"),
            Icon::SquarePlus => SharedString::new_static("icons/square_plus.svg"),
            Icon::SquareMinus => SharedString::new_static("icons/square_minus.svg"),
            Icon::Warning => SharedString::new_static("icons/warning.svg"),
            Icon::FileIcon(path) => SharedString::new_static(path),
        }
    }
}

impl From<Icon> for SharedString {
    fn from(icon: Icon) -> Self {
        icon.path()
    }
}

static FILE_NAME_ICONS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("babel.config.cjs", "icons/file_icons/babel.svg");
    map.insert("babel.config.js", "icons/file_icons/babel.svg");
    map.insert("babel.config.json", "icons/file_icons/babel.svg");
    map.insert("bower.json", "icons/file_icons/bower.svg");
    map.insert("build", "icons/file_icons/bazel.svg");
    map.insert("build.bazel", "icons/file_icons/bazel.svg");
    map.insert("changelog", "icons/file_icons/clock.svg");
    map.insert("changelog.md", "icons/file_icons/clock.svg");
    map.insert("changelog.txt", "icons/file_icons/clock.svg");
    map.insert("changes", "icons/file_icons/clock.svg");
    map.insert("changes.md", "icons/file_icons/clock.svg");
    map.insert("changes.txt", "icons/file_icons/clock.svg");
    map.insert("commit_editmsg", "icons/file_icons/git.svg");
    map.insert("eslint.config.js", "icons/file_icons/eslint.svg");
    map.insert("firebase.json", "icons/file_icons/firebase.svg");
    map.insert("geckodriver", "icons/file_icons/firefox.svg");
    map.insert("gruntfile.babel.js", "icons/file_icons/grunt.svg");
    map.insert("gruntfile.coffee", "icons/file_icons/grunt.svg");
    map.insert("gruntfile.js", "icons/file_icons/grunt.svg");
    map.insert("ionic.config.json", "icons/file_icons/ionic.svg");
    map.insert("ionic.project", "icons/file_icons/ionic.svg");
    map.insert("jenkinsfile", "icons/file_icons/jenkins.svg");
    map.insert("makefile", "icons/file_icons/makefile.svg");
    map.insert("gnumakefile", "icons/file_icons/makefile.svg");
    map.insert("bsdmakefile", "icons/file_icons/makefile.svg");
    map.insert("makefile.am", "icons/file_icons/makefile.svg");
    map.insert("makefile.in", "icons/file_icons/makefile.svg");
    map.insert("karma.conf.cjs", "icons/file_icons/karma.svg");
    map.insert("karma.conf.coffee", "icons/file_icons/karma.svg");
    map.insert("karma.conf.js", "icons/file_icons/karma.svg");
    map.insert("karma.conf.mjs", "icons/file_icons/karma.svg");
    map.insert("merge_msg", "icons/file_icons/git.svg");
    map.insert("mime.types", "icons/file_icons/config.svg");
    map.insert("mvnw", "icons/file_icons/maven.svg");
    map.insert("npm-debug.log", "icons/file_icons/npm_ignored.svg");
    map.insert("platformio.ini", "icons/file_icons/platformio.svg");
    map.insert("pom.xml", "icons/file_icons/maven.svg");
    map.insert("readme", "icons/file_icons/info.svg");
    map.insert("readme.md", "icons/file_icons/info.svg");
    map.insert("readme.txt", "icons/file_icons/info.svg");
    map.insert("rollup.config.js", "icons/file_icons/rollup.svg");
    map.insert("sass-lint.yml", "icons/file_icons/sass.svg");
    map.insert("stylelint.config.cjs", "icons/file_icons/stylelint.svg");
    map.insert("stylelint.config.js", "icons/file_icons/stylelint.svg");
    map.insert("stylelint.config.mjs", "icons/file_icons/stylelint.svg");
    map.insert("swagger.json", "icons/file_icons/json.svg");
    map.insert("swagger.yaml", "icons/file_icons/json.svg");
    map.insert("swagger.yml", "icons/file_icons/json.svg");
    map.insert("tsconfig.json", "icons/file_icons/tsconfig.svg");
    map.insert("version", "icons/file_icons/clock.svg");
    map.insert("version.md", "icons/file_icons/clock.svg");
    map.insert("version.txt", "icons/file_icons/clock.svg");
    map.insert("vite.config.cjs", "icons/file_icons/vite.svg");
    map.insert("vite.config.cts", "icons/file_icons/vite.svg");
    map.insert("vite.config.js", "icons/file_icons/vite.svg");
    map.insert("vite.config.mjs", "icons/file_icons/vite.svg");
    map.insert("vite.config.mts", "icons/file_icons/vite.svg");
    map.insert("vite.config.ts", "icons/file_icons/vite.svg");
    map.insert("webpack.common.cjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.common.js", "icons/file_icons/webpack.svg");
    map.insert("webpack.common.mjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.common.ts", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.build.cjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.build.js", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.build.mjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.build.ts", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.cjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.js", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.mjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.config.ts", "icons/file_icons/webpack.svg");
    map.insert("webpack.dev.cjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.dev.js", "icons/file_icons/webpack.svg");
    map.insert("webpack.dev.mjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.dev.ts", "icons/file_icons/webpack.svg");
    map.insert("webpack.prod.cjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.prod.js", "icons/file_icons/webpack.svg");
    map.insert("webpack.prod.mjs", "icons/file_icons/webpack.svg");
    map.insert("webpack.prod.ts", "icons/file_icons/webpack.svg");
    map.insert("workspace", "icons/file_icons/bazel.svg");
    map.insert("workspace.bazel", "icons/file_icons/bazel.svg");
    map.insert("yarn.clean", "icons/file_icons/yarn.svg");
    map.insert("yarn.lock", "icons/file_icons/yarn.svg");
    map
});

static FILE_EXTENSION_ICONS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("3dm", "icons/file_icons/svg.svg");
    map.insert("3ds", "icons/file_icons/svg.svg");
    map.insert("ad", "icons/file_icons/argdown.svg");
    map.insert("ai", "icons/file_icons/illustrator.svg");
    map.insert("apex", "icons/file_icons/salesforce.svg");
    map.insert("argdown", "icons/file_icons/argdown.svg");
    map.insert("article", "icons/file_icons/go.svg");
    map.insert("asax", "icons/file_icons/html.svg");
    map.insert("ascx", "icons/file_icons/html.svg");
    map.insert("asm", "icons/file_icons/asm.svg");
    map.insert("aspx", "icons/file_icons/html.svg");
    map.insert("avi", "icons/file_icons/video.svg");
    map.insert("avif", "icons/file_icons/image.svg");
    map.insert("babelrc", "icons/file_icons/babel.svg");
    map.insert("babelrc.cjs", "icons/file_icons/babel.svg");
    map.insert("babelrc.js", "icons/file_icons/babel.svg");
    map.insert("bashrc", "icons/file_icons/shell.svg");
    map.insert("bat", "icons/file_icons/windows.svg");
    map.insert("bazel", "icons/file_icons/bazel.svg");
    map.insert("bazelignore", "icons/file_icons/bazel.svg");
    map.insert("bazelrc", "icons/file_icons/bazel.svg");
    map.insert("bazelversion", "icons/file_icons/bazel.svg");
    map.insert("bicep", "icons/file_icons/bicep.svg");
    map.insert("bowerrc", "icons/file_icons/bower.svg");
    map.insert("bsl", "icons/file_icons/bsl.svg");
    map.insert("build", "icons/file_icons/bazel.svg");
    map.insert("bzl", "icons/file_icons/bazel.svg");
    map.insert("c", "icons/file_icons/c.svg");
    map.insert("c++", "icons/file_icons/cpp.svg");
    map.insert("cake", "icons/file_icons/cake.svg");
    map.insert("cc", "icons/file_icons/cpp.svg");
    map.insert("cer", "icons/file_icons/lock.svg");
    map.insert("cert", "icons/file_icons/lock.svg");
    map.insert("cfc", "icons/file_icons/coldfusion.svg");
    map.insert("cfg", "icons/file_icons/config.svg");
    map.insert("cfm", "icons/file_icons/coldfusion.svg");
    map.insert("cjs", "icons/file_icons/javascript.svg");
    map.insert("cjs.map", "icons/file_icons/javascript.svg");
    map.insert("cjsx", "icons/file_icons/react.svg");
    map.insert("class", "icons/file_icons/java.svg");
    map.insert("classpath", "icons/file_icons/java.svg");
    map.insert("clj", "icons/file_icons/clojure.svg");
    map.insert("cljc", "icons/file_icons/clojure.svg");
    map.insert("cljs", "icons/file_icons/clojure.svg");
    map.insert("cls", "icons/file_icons/salesforce.svg");
    map.insert("cmd", "icons/file_icons/windows.svg");
    map.insert("cmx", "icons/file_icons/ocaml.svg");
    map.insert("cmxa", "icons/file_icons/ocaml.svg");
    map.insert("code-search", "icons/file_icons/code-search.svg");
    map.insert("codeclimate.yml", "icons/file_icons/code-climate.svg");
    map.insert("coffee", "icons/file_icons/coffee.svg");
    map.insert("component", "icons/file_icons/html.svg");
    map.insert("conf", "icons/file_icons/config.svg");
    map.insert("config", "icons/file_icons/config.svg");
    map.insert("cpp", "icons/file_icons/cpp.svg");
    map.insert("cr", "icons/file_icons/crystal.svg");
    map.insert("crt", "icons/file_icons/lock.svg");
    map.insert("cs", "icons/file_icons/c-sharp.svg");
    map.insert("cshtml", "icons/file_icons/html.svg");
    map.insert("cson", "icons/file_icons/json.svg");
    map.insert("css", "icons/file_icons/css.svg");
    map.insert("css.map", "icons/file_icons/css.svg");
    map.insert("csv", "icons/file_icons/csv.svg");
    map.insert("ctp", "icons/file_icons/cake_php.svg");
    map.insert("cu", "icons/file_icons/cu.svg");
    map.insert("cuh", "icons/file_icons/cu.svg");
    map.insert("cxx", "icons/file_icons/cpp.svg");
    map.insert("d", "icons/file_icons/d.svg");
    map.insert("dae", "icons/file_icons/svg.svg");
    map.insert("dart", "icons/file_icons/dart.svg");
    map.insert("direnv", "icons/file_icons/config.svg");
    map.insert("doc", "icons/file_icons/word.svg");
    map.insert("docx", "icons/file_icons/word.svg");
    map.insert("ds_store", "icons/file_icons/ignored.svg");
    map.insert("dtx", "icons/file_icons/tex.svg");
    map.insert("ecr", "icons/file_icons/crystal_embedded.svg");
    map.insert("editorconfig", "icons/file_icons/config.svg");
    map.insert("edn", "icons/file_icons/clojure.svg");
    map.insert("ejs", "icons/file_icons/ejs.svg");
    map.insert("elm", "icons/file_icons/elm.svg");
    map.insert("env", "icons/file_icons/config.svg");
    map.insert("eot", "icons/file_icons/font.svg");
    map.insert("epp", "icons/file_icons/puppet.svg");
    map.insert("erb", "icons/file_icons/html_erb.svg");
    map.insert("erb.html", "icons/file_icons/html_erb.svg");
    map.insert("es", "icons/file_icons/javascript.svg");
    map.insert("es5", "icons/file_icons/javascript.svg");
    map.insert("es6", "icons/file_icons/javascript.svg");
    map.insert("es7", "icons/file_icons/javascript.svg");
    map.insert("eslintignore", "icons/file_icons/eslint.svg");
    map.insert("eslintrc", "icons/file_icons/eslint.svg");
    map.insert("eslintrc.cjs", "icons/file_icons/eslint.svg");
    map.insert("eslintrc.js", "icons/file_icons/eslint.svg");
    map.insert("eslintrc.json", "icons/file_icons/eslint.svg");
    map.insert("eslintrc.yaml", "icons/file_icons/eslint.svg");
    map.insert("eslintrc.yml", "icons/file_icons/eslint.svg");
    map.insert("ex", "icons/file_icons/elixir.svg");
    map.insert("exs", "icons/file_icons/elixir_script.svg");
    map.insert("firebaserc", "icons/file_icons/firebase.svg");
    map.insert("fish", "icons/file_icons/shell.svg");
    map.insert("flac", "icons/file_icons/audio.svg");
    map.insert("fs", "icons/file_icons/f-sharp.svg");
    map.insert("fsx", "icons/file_icons/f-sharp.svg");
    map.insert("gd", "icons/file_icons/godot.svg");
    map.insert("gif", "icons/file_icons/image.svg");
    map.insert("gitattributes", "icons/file_icons/git.svg");
    map.insert("gitconfig", "icons/file_icons/git.svg");
    map.insert("github-issues", "icons/file_icons/github.svg");
    map.insert("gitignore", "icons/file_icons/git.svg");
    map.insert("gitkeep", "icons/file_icons/git.svg");
    map.insert("gitlab-ci.yml", "icons/file_icons/gitlab.svg");
    map.insert("gitmodules", "icons/file_icons/git.svg");
    map.insert("go", "icons/file_icons/go2.svg");
    map.insert("godot", "icons/file_icons/godot.svg");
    map.insert("gql", "icons/file_icons/graphql.svg");
    map.insert("gradle", "icons/file_icons/gradle.svg");
    map.insert("graphql", "icons/file_icons/graphql.svg");
    map.insert("graphqls", "icons/file_icons/graphql.svg");
    map.insert("groovy", "icons/file_icons/grails.svg");
    map.insert("gsp", "icons/file_icons/grails.svg");
    map.insert("h", "icons/file_icons/c.svg");
    map.insert("h++", "icons/file_icons/cpp.svg");
    map.insert("hack", "icons/file_icons/hacklang.svg");
    map.insert("haml", "icons/file_icons/haml.svg");
    map.insert("handlebars", "icons/file_icons/mustache.svg");
    map.insert("happenings", "icons/file_icons/happenings.svg");
    map.insert("hbs", "icons/file_icons/mustache.svg");
    map.insert("hh", "icons/file_icons/cpp.svg");
    map.insert("hjs", "icons/file_icons/mustache.svg");
    map.insert("hpp", "icons/file_icons/cpp.svg");
    map.insert("hs", "icons/file_icons/haskell.svg");
    map.insert("htaccess", "icons/file_icons/config.svg");
    map.insert("html", "icons/file_icons/html.svg");
    map.insert("html.erb", "icons/file_icons/html_erb.svg");
    map.insert("hu", "icons/file_icons/cu.svg");
    map.insert("hx", "icons/file_icons/haxe.svg");
    map.insert("hxml", "icons/file_icons/haxe.svg");
    map.insert("hxp", "icons/file_icons/haxe.svg");
    map.insert("hxs", "icons/file_icons/haxe.svg");
    map.insert("hxx", "icons/file_icons/cpp.svg");
    map.insert("ico", "icons/file_icons/favicon.svg");
    map.insert("ins", "icons/file_icons/tex.svg");
    map.insert("ipynb", "icons/file_icons/notebook.svg");
    map.insert("jade", "icons/file_icons/jade.svg");
    map.insert("jar", "icons/file_icons/zip.svg");
    map.insert("java", "icons/file_icons/java.svg");
    map.insert("jinja", "icons/file_icons/jinja.svg");
    map.insert("jinja2", "icons/file_icons/jinja.svg");
    map.insert("jl", "icons/file_icons/julia.svg");
    map.insert("jpeg", "icons/file_icons/image.svg");
    map.insert("jpg", "icons/file_icons/image.svg");
    map.insert("js", "icons/file_icons/javascript.svg");
    map.insert("js.map", "icons/file_icons/javascript.svg");
    map.insert("jscsrc", "icons/file_icons/javascript.svg");
    map.insert("jshintrc", "icons/file_icons/javascript.svg");
    map.insert("json", "icons/file_icons/json.svg");
    map.insert("jsx", "icons/file_icons/react.svg");
    map.insert("key", "icons/file_icons/lock.svg");
    map.insert("kt", "icons/file_icons/kotlin.svg");
    map.insert("kts", "icons/file_icons/kotlin.svg");
    map.insert("less", "icons/file_icons/less.svg");
    map.insert("lhs", "icons/file_icons/haskell.svg");
    map.insert("liquid", "icons/file_icons/liquid.svg");
    map.insert("litcoffee", "icons/file_icons/coffee.svg");
    map.insert("ls", "icons/file_icons/livescript.svg");
    map.insert("lua", "icons/file_icons/lua.svg");
    map.insert("m", "icons/file_icons/c.svg");
    map.insert("mk", "icons/file_icons/makefile.svg");
    map.insert("mak", "icons/file_icons/makefile.svg");
    map.insert("make", "icons/file_icons/makefile.svg");
    map.insert("markdown", "icons/file_icons/markdown.svg");
    map.insert("master", "icons/file_icons/html.svg");
    map.insert("md", "icons/file_icons/markdown.svg");
    map.insert("mdo", "icons/file_icons/mdo.svg");
    map.insert("mjs", "icons/file_icons/javascript.svg");
    map.insert("mjs.map", "icons/file_icons/javascript.svg");
    map.insert("ml", "icons/file_icons/ocaml.svg");
    map.insert("mli", "icons/file_icons/ocaml.svg");
    map.insert("mm", "icons/file_icons/cpp.svg");
    map.insert("mov", "icons/file_icons/video.svg");
    map.insert("mp3", "icons/file_icons/audio.svg");
    map.insert("mp4", "icons/file_icons/video.svg");
    map.insert("mpg", "icons/file_icons/video.svg");
    map.insert("mustache", "icons/file_icons/mustache.svg");
    map.insert("nim", "icons/file_icons/nim.svg");
    map.insert("nims", "icons/file_icons/nim.svg");
    map.insert("nj", "icons/file_icons/nunjucks.svg");
    map.insert("njk", "icons/file_icons/nunjucks.svg");
    map.insert("njs", "icons/file_icons/nunjucks.svg");
    map.insert("npm-debug.log", "icons/file_icons/npm.svg");
    map.insert("npmignore", "icons/file_icons/npm.svg");
    map.insert("npmrc", "icons/file_icons/npm.svg");
    map.insert("nunj", "icons/file_icons/nunjucks.svg");
    map.insert("nunjs", "icons/file_icons/nunjucks.svg");
    map.insert("nunjucks", "icons/file_icons/nunjucks.svg");
    map.insert("obj", "icons/file_icons/svg.svg");
    map.insert("odata", "icons/file_icons/odata.svg");
    map.insert("ogg", "icons/file_icons/audio.svg");
    map.insert("ogv", "icons/file_icons/video.svg");
    map.insert("otf", "icons/file_icons/font.svg");
    map.insert("pddl", "icons/file_icons/pddl.svg");
    map.insert("pdf", "icons/file_icons/pdf.svg");
    map.insert("pem", "icons/file_icons/lock.svg");
    map.insert("php", "icons/file_icons/php.svg");
    map.insert("php.inc", "icons/file_icons/php.svg");
    map.insert("pipeline", "icons/file_icons/pipeline.svg");
    map.insert("pl", "icons/file_icons/perl.svg");
    map.insert("plan", "icons/file_icons/plan.svg");
    map.insert("png", "icons/file_icons/image.svg");
    map.insert("pp", "icons/file_icons/puppet.svg");
    map.insert("prisma", "icons/file_icons/prisma.svg");
    map.insert("pro", "icons/file_icons/prolog.svg");
    map.insert("properties", "icons/file_icons/java.svg");
    map.insert("ps1", "icons/file_icons/powershell.svg");
    map.insert("psd", "icons/file_icons/photoshop.svg");
    map.insert("psd1", "icons/file_icons/powershell.svg");
    map.insert("psm1", "icons/file_icons/powershell.svg");
    map.insert("pug", "icons/file_icons/pug.svg");
    map.insert("purs", "icons/file_icons/purescript.svg");
    map.insert("pxm", "icons/file_icons/image.svg");
    map.insert("py", "icons/file_icons/python.svg");
    map.insert("r", "icons/file_icons/R.svg");
    map.insert("rb", "icons/file_icons/ruby.svg");
    map.insert("re", "icons/file_icons/reasonml.svg");
    map.insert("res", "icons/file_icons/rescript.svg");
    map.insert("resi", "icons/file_icons/rescript.svg");
    map.insert("rmd", "icons/file_icons/R.svg");
    map.insert("rs", "icons/file_icons/rust.svg");
    map.insert("s", "icons/file_icons/asm.svg");
    map.insert("sass", "icons/file_icons/sass.svg");
    map.insert("sbt", "icons/file_icons/sbt.svg");
    map.insert("scala", "icons/file_icons/scala.svg");
    map.insert("scss", "icons/file_icons/sass.svg");
    map.insert("sh", "icons/file_icons/shell.svg");
    map.insert("slang", "icons/file_icons/crystal_embedded.svg");
    map.insert("slide", "icons/file_icons/go.svg");
    map.insert("slim", "icons/file_icons/slim.svg");
    map.insert("slugignore", "icons/file_icons/config.svg");
    map.insert("smarty.tpl", "icons/file_icons/smarty.svg");
    map.insert("sol", "icons/file_icons/ethereum.svg");
    map.insert("soql", "icons/file_icons/db.svg");
    map.insert("spec.cjs", "icons/file_icons/javascript.svg");
    map.insert("spec.js", "icons/file_icons/javascript.svg");
    map.insert("spec.jsx", "icons/file_icons/react.svg");
    map.insert("spec.mjs", "icons/file_icons/javascript.svg");
    map.insert("spec.ts", "icons/file_icons/typescript.svg");
    map.insert("spec.tsx", "icons/file_icons/react.svg");
    map.insert("springbeans", "icons/file_icons/spring.svg");
    map.insert("sql", "icons/file_icons/db.svg");
    map.insert("sss", "icons/file_icons/css.svg");
    map.insert("stache", "icons/file_icons/mustache.svg");
    map.insert("static", "icons/file_icons/config.svg");
    map.insert("stl", "icons/file_icons/svg.svg");
    map.insert("sty", "icons/file_icons/tex.svg");
    map.insert("styl", "icons/file_icons/stylus.svg");
    map.insert("stylelintignore", "icons/file_icons/stylelint.svg");
    map.insert("stylelintrc", "icons/file_icons/stylelint.svg");
    map.insert("stylelintrc.js", "icons/file_icons/stylelint.svg");
    map.insert("stylelintrc.json", "icons/file_icons/stylelint.svg");
    map.insert("stylelintrc.yaml", "icons/file_icons/stylelint.svg");
    map.insert("stylelintrc.yml", "icons/file_icons/stylelint.svg");
    map.insert("sublime-project", "icons/file_icons/sublime.svg");
    map.insert("sublime-workspace", "icons/file_icons/sublime.svg");
    map.insert("svelte", "icons/file_icons/svelte.svg");
    map.insert("svg", "icons/file_icons/svg.svg");
    map.insert("svgx", "icons/file_icons/image.svg");
    map.insert("swift", "icons/file_icons/swift.svg");
    map.insert("test.cjs", "icons/file_icons/javascript.svg");
    map.insert("test.js", "icons/file_icons/javascript.svg");
    map.insert("test.jsx", "icons/file_icons/react.svg");
    map.insert("test.mjs", "icons/file_icons/javascript.svg");
    map.insert("test.ts", "icons/file_icons/typescript.svg");
    map.insert("test.tsx", "icons/file_icons/react.svg");
    map.insert("tex", "icons/file_icons/tex.svg");
    map.insert("tf", "icons/file_icons/terraform.svg");
    map.insert("tf.json", "icons/file_icons/terraform.svg");
    map.insert("tfvars", "icons/file_icons/terraform.svg");
    map.insert("tfvars.json", "icons/file_icons/terraform.svg");
    map.insert("tiff", "icons/file_icons/image.svg");
    map.insert("tmp", "icons/file_icons/clock.svg");
    map.insert("toml", "icons/file_icons/config.svg");
    map.insert("tpl", "icons/file_icons/smarty.svg");
    map.insert("tres", "icons/file_icons/godot.svg");
    map.insert("ts", "icons/file_icons/typescript.svg");
    map.insert("tscn", "icons/file_icons/godot.svg");
    map.insert("tsx", "icons/file_icons/react.svg");
    map.insert("ttf", "icons/file_icons/font.svg");
    map.insert("twig", "icons/file_icons/twig.svg");
    map.insert("txt", "icons/file_icons/default.svg");
    map.insert("vala", "icons/file_icons/vala.svg");
    map.insert("vapi", "icons/file_icons/vala.svg");
    map.insert("vue", "icons/file_icons/vue.svg");
    map.insert("wasm", "icons/file_icons/wasm.svg");
    map.insert("wat", "icons/file_icons/wat.svg");
    map.insert("wav", "icons/file_icons/audio.svg");
    map.insert("webm", "icons/file_icons/video.svg");
    map.insert("webp", "icons/file_icons/image.svg");
    map.insert("wgt", "icons/file_icons/wgt.svg");
    map.insert("woff", "icons/file_icons/font.svg");
    map.insert("woff2", "icons/file_icons/font.svg");
    map.insert("workspace", "icons/file_icons/bazel.svg");
    map.insert("xls", "icons/file_icons/xls.svg");
    map.insert("xlsx", "icons/file_icons/xls.svg");
    map.insert("xml", "icons/file_icons/xml.svg");
    map.insert("yaml", "icons/file_icons/yml.svg");
    map.insert("yml", "icons/file_icons/yml.svg");
    map.insert("zig", "icons/file_icons/zig.svg");
    map.insert("zip", "icons/file_icons/zip.svg");
    map.insert("zsh", "icons/file_icons/shell.svg");
    map.insert("zshrc", "icons/file_icons/shell.svg");
    map
});

/// Returns the icon for the given file, or `None` if no specific icon is known.
pub fn file_icon(path: &Path) -> Option<Icon> {
    let file_name = path.file_name()?.to_str()?;
    let lower_name = file_name.to_lowercase();

    // 1. Exact file name match (e.g., Dockerfile, Makefile, README)
    if let Some(icon_path) = FILE_NAME_ICONS.get(lower_name.as_str()) {
        return Some(Icon::FileIcon(icon_path));
    }

    // 2. Suffix match for compound extensions (e.g., vite.config.js -> config.js, js)
    let mut suffix = lower_name.as_str();
    while let Some((_, rest)) = suffix.split_once('.') {
        suffix = rest;
        if let Some(icon_path) = FILE_EXTENSION_ICONS.get(suffix) {
            return Some(Icon::FileIcon(icon_path));
        }
    }

    None
}

/// Returns the file icon or the generic file icon if no specific icon is known.
pub fn file_icon_or_default(path: &Path) -> Icon {
    file_icon(path).unwrap_or(Icon::File)
}
