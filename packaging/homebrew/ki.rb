# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.1. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.1/ki-v0.4.1-aarch64-apple-darwin.tar.gz"
      sha256 "f7f894faa5a7d4153cb3cc7a709a9792dd1fbff16d2d19445f5455640dc3ceec"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.1/ki-v0.4.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "4e009eca175c3d1c67c355180e5669198c87fcc829a7ffcc616ff33067369423"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.1/ki-v0.4.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "4c5f91c8654dc92316d07cc2af8f48f23fdb9aed09b2b282c986ef928b6dd022"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
