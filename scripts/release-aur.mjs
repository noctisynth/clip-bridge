// @ts-check

import { execSync } from "node:child_process";
import {
	existsSync,
	mkdirSync,
	readFileSync,
	unlinkSync,
	writeFileSync,
} from "node:fs";
import path from "node:path";

const projectRoot = process.cwd();

execSync("mkdir -p release", { stdio: "inherit" });
process.chdir("release");
console.log(`Current directory: ${process.cwd()}`);

const basePath = process.cwd();
const homePath = process.env.HOME ?? basePath;
const sshPath = path.resolve(homePath, ".ssh");
if (!existsSync(sshPath)) {
	mkdirSync(sshPath, { recursive: true });
}

// Check if AUR_SSH_KEY environment variable is set
const AUR_SSH_KEY = process.env.AUR_SSH_KEY;
if (!AUR_SSH_KEY) {
	console.error("AUR_SSH_KEY environment variable is not set.");
	process.exit(1);
}

// Remove old SSH key file if it exists
const aurSSHKeyPath = path.resolve(sshPath, "aur");
if (existsSync(aurSSHKeyPath)) {
	unlinkSync(aurSSHKeyPath);
}

// Write new SSH key file
writeFileSync(aurSSHKeyPath, `${AUR_SSH_KEY}\n`);
execSync(`chmod 400 ${aurSSHKeyPath}`);

// Add aur to known hosts
const knownHostsPath = path.resolve(sshPath, "known_hosts");
if (existsSync(knownHostsPath)) {
	const knownHosts = readFileSync(knownHostsPath, {
		encoding: "utf-8",
	});
	if (!knownHosts.includes("aur.archlinux.org")) {
		execSync(
			`ssh-keyscan -v -t "rsa,ecdsa,ed25519" aur.archlinux.org >> ~/.ssh/known_hosts`,
			{ stdio: "inherit" },
		);
	}
} else {
	execSync(
		`ssh-keyscan -v -t "rsa,ecdsa,ed25519" aur.archlinux.org > ~/.ssh/known_hosts`,
		{ stdio: "inherit" },
	);
}

// Clone AUR repository if not exists
if (!existsSync("aur")) {
	execSync(
		`git -c init.defaultBranch=master -c core.sshCommand="ssh -i ${aurSSHKeyPath}" clone ssh://aur@aur.archlinux.org/clip-bridge-bin.git aur`,
		{ stdio: "inherit" },
	);
}
execSync(`git -C aur config core.sshCommand "ssh -i ${aurSSHKeyPath}"`, {
	stdio: "inherit",
});

// Copy files from `target/cargo-aur` to `aur`
execSync("cp -r target/cargo-aur/* release/aur/", {
	stdio: "inherit",
	cwd: projectRoot,
});

// Fix PKGBUILD
// From `https://github.com/noctisynth/clip-bridge/releases/download/v$pkgver/clip-bridge-$pkgver-x86_64.tar.gz`
// to `https://github.com/noctisynth/clip-bridge/releases/download/clip-bridge-v$pkgver/clip-bridge-$pkgver-x86_64.tar.gz`
const pkgbuildPath = path.resolve("aur", "PKGBUILD");
if (!existsSync(pkgbuildPath)) {
	console.error("PKGBUILD file not found.");
	process.exit(1);
}
let pkgbuild = readFileSync(pkgbuildPath, { encoding: "utf-8" });
pkgbuild = pkgbuild.replace(
	"https://github.com/noctisynth/clip-bridge/releases/download/v$pkgver/clip-bridge-$pkgver-x86_64.tar.gz",
	"https://github.com/noctisynth/clip-bridge/releases/download/clip-bridge-v$pkgver/clip-bridge-$pkgver-x86_64.tar.gz",
);
writeFileSync(pkgbuildPath, pkgbuild);

// Generate .SRCINFO file
execSync("makepkg --printsrcinfo > .SRCINFO", {
	cwd: "aur",
	stdio: "inherit",
});

// Setup Git repository
execSync("git add PKGBUILD .SRCINFO", {
	stdio: "inherit",
	cwd: "aur",
});
execSync(`git -C aur config user.name "苏向夜"`, { stdio: "inherit" });
execSync(`git -C aur config user.email "fu050409@163.com"`, {
	stdio: "inherit",
});

// Test AUR package (skip in CI)
if (!process.env.CI) {
	execSync("makepkg -f", {
		stdio: "inherit",
		cwd: "aur",
	});
}

// Publish to AUR
execSync(`git commit -m "release: publish aur"`, {
	stdio: "inherit",
	cwd: "aur",
});
execSync(`git push origin master`, {
	stdio: "inherit",
	cwd: "aur",
});
