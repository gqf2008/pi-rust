# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.3. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.3/ki-v0.5.3-aarch64-apple-darwin.tar.gz"
      sha256 "beb7d34df4f64050a49786e7b96b9615ff40407fcd66dec8f3665c97a2494942"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.3/ki-v0.5.3-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "cf0ea7359a7d99e1990c93fc55202944fcffdef57008df6ee71c90620ce3edcb"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.3/ki-v0.5.3-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "73e40f30442867f526685ecdfe0b720255d33d340feb1fc59e650e4d97bcbacf"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
