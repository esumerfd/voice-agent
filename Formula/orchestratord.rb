class Orchestratord < Formula
  desc "Local voice-agent orchestrator daemon, CLI, and TUI"
  homepage "https://github.com/esumerfd/voice-agent"
  version "0.0.1"
  license "MIT"

  depends_on :macos

  # Tag path is `orchestrator-v#{version}`, NOT a bare `v#{version}` --
  # esumerfd/actions' shared release.yml tags per-product
  # (`<product-name>-v<version>`), since a repo can release more than one
  # independently-versioned product. voice-agent exposes exactly one
  # ("orchestrator": apps/orchestrator/Makefile is its only `release:`
  # target), but the tag still carries the product name. version/sha256
  # below still point at the last release cut under the OLD bare-tag
  # scheme (v0.0.1) -- both need a real update once the first release
  # under the new pipeline actually ships; a fabricated checksum here
  # would be worse than an honestly stale one.
  on_macos do
    on_intel do
      url "https://github.com/esumerfd/voice-agent/releases/download/orchestrator-v#{version}/voice-agent-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1c6090e50e5d6c83c3177d0d255e93eabaa8c780700503a3775fa2ce235ef98e"
    end
  end

  def install
    bin.install "orchestratord", "orchestrator", "orchestrator-tui"
  end

  # activity/agent-runs/agent-scratch land under XDG_DATA_HOME (falling
  # back to ~/.local/share per the XDG Base Directory spec), matching the
  # same convention `make install`'s launchd/systemd templates use --
  # instead of the daemon's own CWD-relative ./data default, which a
  # brew-installed service has no meaningful repo-checkout CWD to resolve
  # against in the first place. Each store is created lazily on first
  # write, so nothing needs pre-creating here.
  #
  # `service do` is instance_eval'd against a Homebrew::Service object, NOT
  # this Formula instance (confirmed the hard way: calling a custom Formula
  # method from here raised "undefined local variable or method" at
  # `brew services start` time) -- only a fixed whitelist of formula path
  # helpers (bin/var/opt_bin/etc) is available inside this block, so the
  # XDG resolution must be inlined here with plain Ruby (ENV/Dir.home)
  # rather than calling out to a Formula-defined method.
  service do
    xdg_data_home = ENV["XDG_DATA_HOME"]
    data_dir = "#{xdg_data_home.presence || "#{Dir.home}/.local/share"}/orchestratord"

    run [opt_bin/"orchestratord"]
    environment_variables ORCHESTRATOR_ACTIVITY_DIR:      "#{data_dir}/activity",
                          ORCHESTRATOR_AGENT_RUNS_DIR:    "#{data_dir}/agent-runs",
                          ORCHESTRATOR_AGENT_SCRATCH_DIR: "#{data_dir}/agent-scratch"
    keep_alive true
    log_path var/"log/orchestratord.log"
    error_log_path var/"log/orchestratord.log"
  end

  test do
    assert_match "Usage: orchestratord", shell_output("#{bin}/orchestratord --help")
    assert_match "Usage: orchestrator", shell_output("#{bin}/orchestrator --help")
    assert_match "Usage: orchestrator-tui", shell_output("#{bin}/orchestrator-tui --help")
  end
end
