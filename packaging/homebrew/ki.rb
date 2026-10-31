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
      sha256 "a6277a7d2b21013ef612d2b78c4ea37d6054fafc23616c108ccfd5045fb11494"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.7/ki-v0.4.7-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0da0cd424977225b5a3d8b0227fac47f4c1bbe31acad480de7cc284ff6637a5a"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.4.7/ki-v0.4.7-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "b5223b80cd23a773f90226b334e1f20bbf6c04117486fa9aade6b1fcc8c174b0"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
