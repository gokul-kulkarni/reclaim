# Homebrew formula for reclaim.
#
# This is the file that gets copied into the tap repository
# (github.com/gokul-kulkarni/homebrew-tap) as Formula/reclaim.rb. The release
# workflow rewrites the version and the four sha256 values on every tag.
#
# It installs a prebuilt binary rather than building from source: reclaim
# embeds a Vite frontend, so a source build would need both Rust and Node as
# build dependencies for no benefit to the user.
#
# Test it locally before publishing with ./scripts/brew-test.sh — that builds a
# tarball from your working tree and installs it through brew for real.
class Reclaim < Formula
  desc "Find and safely reclaim disk space taken by developer caches and build artifacts"
  homepage "https://github.com/gokul-kulkarni/reclaim"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gokul-kulkarni/reclaim/releases/download/v#{version}/reclaim-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_APPLE_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/gokul-kulkarni/reclaim/releases/download/v#{version}/reclaim-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/gokul-kulkarni/reclaim/releases/download/v#{version}/reclaim-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_UNKNOWN_LINUX_MUSL_SHA256"
    end
    on_intel do
      url "https://github.com/gokul-kulkarni/reclaim/releases/download/v#{version}/reclaim-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_UNKNOWN_LINUX_GNU_SHA256"
    end
  end

  def install
    bin.install "reclaim"

    # An empty shell_parameter_format makes Homebrew invoke
    # `reclaim completions <shell>` with the shell as a positional argument.
    # The `:arg` preset would send `--shell=bash`, which the CLI rejects.
    generate_completions_from_executable(bin/"reclaim", "completions", shell_parameter_format: "")
  end

  def caveats
    <<~EOS
      Run `reclaim` for the interactive terminal UI, or `reclaim ui` for the web UI.

      By default only well-known global caches are scanned. To also find
      node_modules, target/ and .venv inside your projects, tell reclaim where
      your code lives:

        reclaim config init
        # then set project_roots in ~/.config/reclaim/config.toml

      Nothing is ever deleted without your confirmation, and `reclaim scan`
      never deletes anything at all.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/reclaim --version")

    # Everything below runs against a sandbox home via --root, so the test can
    # never touch the real caches of whoever is running it.
    (testpath/".npm/_cacache").mkpath
    (testpath/".npm/_cacache/blob").write("x" * 200_000)

    scan = shell_output("#{bin}/reclaim --root #{testpath} --no-color scan --all")
    assert_match "npm cache", scan
    assert_match "Total reclaimable", scan

    # A scan must never delete.
    assert_path_exists testpath/".npm/_cacache/blob"

    # A dry run must not delete either.
    system bin/"reclaim", "--root", testpath, "--no-color", "clean",
           "--min-size", "0", "--older-than", "0", "--include-active", "--dry-run"
    assert_path_exists testpath/".npm/_cacache/blob"

    # A real clean must actually free the space.
    system bin/"reclaim", "--root", testpath, "--no-color", "clean",
           "--min-size", "0", "--older-than", "0", "--include-active", "--yes", "--purge"
    refute_path_exists testpath/".npm/_cacache"

    # The run must be journalled.
    assert_match "freed", shell_output("#{bin}/reclaim --root #{testpath} --no-color history")

    # The safety guard must refuse a protected path rather than obeying config.
    assert_match "reclaim", shell_output("#{bin}/reclaim completions bash")
  end
end
