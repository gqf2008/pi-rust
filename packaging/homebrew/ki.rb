# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.6. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.6"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.6/ki-v0.4.6-aarch64-apple-darwin.tar.gz"
      sha256 "9e93acd0ee075e568213209c9e571b5916a5719fabc4d7196f632233bdd6a765"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.6/ki-v0.4.6-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1194680c147240fdadb2629d9dd3cb2859d27168285cfd080a020101dbe69a96"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.6/ki-v0.4.6-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "»   digest unavailable for ki-v0.4.6-x86_64-unknown-linux-gnu.tar.gz; falling back to the .sha256 sidecar
7e10c603a5c9d9ea2827008341dde3e577ce257e2e35f66bdd8b6ba663db96a4"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
