class Orchestratord < Formula
  desc "Local voice-agent orchestrator daemon, CLI, and TUI"
  homepage "https://github.com/esumerfd/voice-agent"
  version "0.0.1"
  license "MIT"

  depends_on :macos

  on_macos do
    on_intel do
      url "https://github.com/esumerfd/voice-agent/releases/download/v0.0.1/voice-agent-v0.0.1-x86_64-apple-darwin.tar.gz"
      sha256 "1c6090e50e5d6c83c3177d0d255e93eabaa8c780700503a3775fa2ce235ef98e"
    end
  end

  def install
    bin.install "orchestratord", "orchestrator", "orchestrator-tui"
    (var/"orchestratord").mkpath
  end

  test do
    assert_match "Usage: orchestratord", shell_output("#{bin}/orchestratord --help")
    assert_match "Usage: orchestrator", shell_output("#{bin}/orchestrator --help")
    assert_match "Usage: orchestrator-tui", shell_output("#{bin}/orchestrator-tui --help")
  end
end
