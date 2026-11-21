// Prints the Homebrew formula for a release, from the SHA256SUMS the release workflow publishes.
//
//   node release/formula.mjs <version> <SHA256SUMS>

import { readFileSync } from "node:fs";

const [version, sumsPath] = process.argv.slice(2);
if (!version || !sumsPath) {
    console.error("Usage: node release/formula.mjs <version> <SHA256SUMS>");
    process.exit(1);
}

const sums = new Map(
    readFileSync(sumsPath, "utf8")
        .trim()
        .split("\n")
        .map((line) => {
            const [hash, file] = line.trim().split(/\s+\*?/);
            return [file, hash];
        }),
);

const archive = (target) => {
    const file = `bias-${target}.tar.gz`;
    const sha256 = sums.get(file);
    if (!sha256 || !/^[0-9a-f]{64}$/.test(sha256))
        throw new Error(`SHA256SUMS has no hash for ${file}`);
    return [
        `      url "https://github.com/BiasPay/cli/releases/download/v${version}/${file}"`,
        `      sha256 "${sha256}"`,
    ].join("\n");
};

process.stdout
    .write(`# Written by the release workflow in BiasPay/cli; changes here are overwritten.
class Bias < Formula
  desc "Command-line interface for the Bias payments API"
  homepage "https://biaspay.com/docs"
  version "${version}"
  license "MIT"

  on_macos do
    on_arm do
${archive("aarch64-apple-darwin")}
    end
    on_intel do
${archive("x86_64-apple-darwin")}
    end
  end

  on_linux do
    on_arm do
${archive("aarch64-unknown-linux-musl")}
    end
    on_intel do
${archive("x86_64-unknown-linux-musl")}
    end
  end

  def install
    bin.install "bias"
    generate_completions_from_executable(bin/"bias", "completion")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/bias version")
  end
end
`);
