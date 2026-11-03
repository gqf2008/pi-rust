# Homebrew formula for ki.
#
# Not in homebrew-core: that requires notability (stars/forks) ki does not have
# yet. It is prepared for a tap under this project's owner: publish this file
# as `Formula/ki.rb` in `gqf2008/homebrew-tap` first, then:
#
#   brew tap gqf2008/tap
#   brew install ki
#
# Hashes below are for v0.5.2. See packaging/README.md for the refresh procedure.
class Ki < Formula
  desc "Rust-native coding-agent runtime and terminal CLI"
  homepage "https://github.com/gqf2008/pi-rust"
  version "0.5.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.2/ki-v0.5.2-aarch64-apple-darwin.tar.gz"
      sha256 "111e51905a79f16f5def5b6fd114297200275f865ef2c01beb729d797c6f687a"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.2/ki-v0.5.2-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d83bad461e8351eb1b7c5adfb70837adfede98a627dffa17f9f5c33b66e7ed35"
    end
    on_intel do
      url "https://github.com/gqf2008/pi-rust/releases/download/v0.5.2/ki-v0.5.2-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "4d1d04f0ba0add8fdee4da0b62bde8b27a044cead57e68a634571eb4e0416614"
    end
  end

  def install
    bin.install "ki"
  end

  test do
    assert_match "ki", shell_output("#{bin}/ki --version")
  end
end
