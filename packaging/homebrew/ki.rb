# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.5. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.5/ki-v0.4.5-aarch64-apple-darwin.tar.gz"
      sha256 "967752da901a5f9222cf903ecc82c8915376da26f93e9e8dea1e39258c56cdd9"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.5/ki-v0.4.5-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "6a832155f24de25056667af93e26cc2cf6e477e38c15b5c6243f6f606dbde52c"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.5/ki-v0.4.5-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "99da179616625dccc42c6de21b0b8f800dcc6fc3f1cedd0e95fc557a5778f7fd"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
