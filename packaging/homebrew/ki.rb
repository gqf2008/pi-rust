# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.6. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.6"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.6/ki-v0.5.6-aarch64-apple-darwin.tar.gz"
      sha256 "f69bebeabdd3d81f73e2cc0bbf29024a334801604b5897ed759256f2d3a57241"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.6/ki-v0.5.6-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f885e462fe1a7b77b1ac7ba427d7970a3546e0045a62da8a4343ab0998895347"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.6/ki-v0.5.6-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0477c348c1c73f97a05e6c4df58021c953e1101367b0d7d9cbbdcb8013e67eff"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
