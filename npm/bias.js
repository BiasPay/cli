#!/usr/bin/env node
// Runs the bias binary from the package for this platform, which npm installs as an optional
// dependency of @biaspay/cli. release/npm.mjs writes the platform table below into the copy it publishes.
"use strict";

const { spawnSync } = require("node:child_process");

const PLATFORMS = {};

const name = PLATFORMS[`${process.platform} ${process.arch}`];
if (!name) {
    console.error(
        `bias: there's no build for ${process.platform} ${process.arch}. ` +
            "Download one from https://github.com/BiasPay/cli/releases if your platform is listed.",
    );
    process.exit(1);
}

let binary;
try {
    binary = require.resolve(`${name}/bin/bias${process.platform === "win32" ? ".exe" : ""}`);
} catch {
    console.error(
        `bias: ${name} isn't installed. Reinstall @biaspay/cli without --no-optional or --omit=optional.`,
    );
    process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
    console.error(`bias: ${result.error.message}`);
    process.exit(1);
}
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
