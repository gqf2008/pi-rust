# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.4. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.4/ki-v0.5.4-aarch64-apple-darwin.tar.gz"
      sha256 "b73deb91c6d6835c9ecc1dd43edf81783e2e1897358b26fe5f21d96cf14871fd"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.4/ki-v0.5.4-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "baa00823c118c25241658e3aa3e254bf3555303709b48247012bc3173ef7a82b"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.4/ki-v0.5.4-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "49c8e164f1773a5ee1efc769543e00cab77fd39d8728accf1e4b63f36440d285"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
