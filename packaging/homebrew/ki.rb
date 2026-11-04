# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.5. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.5/ki-v0.5.5-aarch64-apple-darwin.tar.gz"
      sha256 "80e47d7a4039203644affc72b0f17b15b33523727f1c16b38e4b0acaf4ee9e74"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.5/ki-v0.5.5-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "12f8948cf5d4390914f19ccda773552cc618b7e517f145e6316881bc345c2670"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.5/ki-v0.5.5-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cb7b7730ca34d3cc32c3aa226c6340058b483e2037e2e501bf873d0febfec03a"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
