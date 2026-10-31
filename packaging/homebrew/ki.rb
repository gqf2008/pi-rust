# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.4.7. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://ki.laofu.online/"
  version "0.4.7"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.7/ki-v0.4.7-aarch64-apple-darwin.tar.gz"
      sha256 "99d43eb1da9ba0a8e263492a0fe5e942cef804118fc6272e861e15bf6e593dfe"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.7/ki-v0.4.7-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "e327c7e77ac941067a5dbe81b526f9b35316d86912ab654ba15cd07f12dc60c4"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.7/ki-v0.4.7-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "15052b950f786b299ac65655936001e26c79287060077fb3df34fd2ff87b36ca"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
