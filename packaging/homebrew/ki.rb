# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.0. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.0/ki-v0.5.0-aarch64-apple-darwin.tar.gz"
      sha256 "51427a4543c5a7faf1b2d1b52237b09a1423d523931bab57d3794cb148015b02"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.0/ki-v0.5.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9ebc51f6ee0c41e6eeff5a29cbb3f57d2d0b4219dbc547a377761b1f2c88494e"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.0/ki-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "87bd4fe6dbe1b7097c10c160aad9892cc4842c37e9a97b3f04a4c3735a32c087"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
