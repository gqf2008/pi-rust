# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.1. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.1/ki-v0.5.1-aarch64-apple-darwin.tar.gz"
      sha256 "a7b7d6744d4a6214c0acc4c396779638ae123fa2d39aab2251f41b4c585da50c"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.1/ki-v0.5.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ce7e35de72dbd09c23d3b4c1e227a5dce96075f56908f697e063d9411b662b39"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.1/ki-v0.5.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cf45890d7ffffafa3aa1ae23e33cc0371d5bec2559e5fd6183261f4a7e3f0788"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
