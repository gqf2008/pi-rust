# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.2. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.2/ki-v0.4.2-aarch64-apple-darwin.tar.gz"
      sha256 "90ca000047a0ab8fa6ed39dd82befdbcda0fdb8f4a0220c46575c95bb829f63c"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.2/ki-v0.4.2-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "aac25945c7c9f9228679e41bf2dacc2d0943eb5d738e8bd0bb88cbefd49df315"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.2/ki-v0.4.2-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ef9bf1d4042a9344b26aa652cfbe42b5759fe0bbb529bf820b63e85380b4016e"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
