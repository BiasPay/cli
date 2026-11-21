// Writes the npm packages for a release: one per platform holding its binary, and @biaspay/cli,
// which depends on them all optionally so npm installs only the one whose os and cpu match.
//
//   node release/npm.mjs <version> <binaries> <out>
//
// <binaries>/<target>/ holds each target's bias (bias.exe on Windows). Prints the package
// directories in publish order, platforms first, so @biaspay/cli never points at a missing version.

import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const [version, binaries, out] = process.argv.slice(2);
if (!version || !binaries || !out) {
    console.error("Usage: node release/npm.mjs <version> <binaries> <out>");
    process.exit(1);
}

const targets = JSON.parse(readFileSync(join(root, "release/targets.json"), "utf8"));
const common = {
    version,
    license: "MIT",
    homepage: "https://biaspay.com/docs",
    repository: { type: "git", url: "git+https://github.com/BiasPay/cli.git" },
};
const write = (dir, manifest) =>
    writeFileSync(join(dir, "package.json"), `${JSON.stringify(manifest, null, 4)}\n`);

const platforms = {};
const directories = [];
for (const [target, { os, cpu }] of Object.entries(targets)) {
    const name = `@biaspay/cli-${os}-${cpu}`;
    const executable = os === "win32" ? "bias.exe" : "bias";
    const dir = join(out, `cli-${os}-${cpu}`);
    mkdirSync(join(dir, "bin"), { recursive: true });
    copyFileSync(join(binaries, target, executable), join(dir, "bin", executable));
    chmodSync(join(dir, "bin", executable), 0o755);
    copyFileSync(join(root, "LICENSE"), join(dir, "LICENSE"));
    write(dir, {
        name,
        ...common,
        description: `The Bias CLI's ${target} binary, installed by @biaspay/cli`,
        os: [os],
        cpu: [cpu],
        files: ["bin"],
        preferUnplugged: true,
    });
    platforms[`${os} ${cpu}`] = name;
    directories.push(dir);
}

const dir = join(out, "cli");
mkdirSync(join(dir, "bin"), { recursive: true });
const launcher = readFileSync(join(root, "npm/bias.js"), "utf8");
const table = `const PLATFORMS = ${JSON.stringify(platforms, null, 4)};`;
if (!launcher.includes("const PLATFORMS = {};"))
    throw new Error("npm/bias.js has no platform table");
writeFileSync(join(dir, "bin/bias.js"), launcher.replace("const PLATFORMS = {};", table));
chmodSync(join(dir, "bin/bias.js"), 0o755);
copyFileSync(join(root, "LICENSE"), join(dir, "LICENSE"));
copyFileSync(join(root, "README.md"), join(dir, "README.md"));
write(dir, {
    name: "@biaspay/cli",
    ...common,
    description: "Command-line interface for the Bias payments API",
    bin: { bias: "bin/bias.js" },
    files: ["bin"],
    engines: { node: ">=18" },
    optionalDependencies: Object.fromEntries(
        Object.values(platforms).map((name) => [name, version]),
    ),
});
directories.push(dir);

console.log(directories.join("\n"));
