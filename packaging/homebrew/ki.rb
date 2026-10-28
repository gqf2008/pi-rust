# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.4. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.4/ki-v0.4.4-aarch64-apple-darwin.tar.gz"
      sha256 "1a99c43c6f158e3cb437724a501cbf654edb79f14f1b05df6d1991aee9e1db00"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.4/ki-v0.4.4-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "5dd459b6968c64438f2f4b16cb8142f939bfad8ae72302e4729feb6d2c5a6ac7"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.4/ki-v0.4.4-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d5ec1b404ffad0e0d0e3a32f43ae70334922a71e1a156a16ce66ed092c2c3a1a"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
