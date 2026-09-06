// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

// https://astro.build/config
//
// Petal Docs (petal.live/docs) — end-user + self-hosting-operator documentation.
// NOT a contributor/internals site: no architecture, wire-protocol, or
// release-engineering content belongs here. See site/README.md for the page
// inventory and the AUTO/OWNED content-ownership model.
export default defineConfig({
	site: 'https://petal.live',
	base: '/docs',
	// `base` only prefixes internal link/asset URLs — it does NOT nest the
	// physical build output under a docs/ folder. We deploy this as its own
	// Vercel project, proxied from petal-website's vercel.json at /docs/:path*,
	// so the served path (https://<project>.vercel.app/docs/...) must match a
	// real file on disk. outDir puts the whole build under dist/docs/ to make
	// that true (dist/docs/index.html, dist/docs/_astro/..., etc).
	outDir: './dist/docs',
	integrations: [
		starlight({
			title: 'Petal Docs',
			description:
				'Documentation for Petal, low-latency multi-window screenshare for macOS.',
			// Brand alignment with petal-website (petal.live): self-hosted
			// Manrope/Albert Sans/JetBrains Mono + the same petal-mark favicon,
			// see src/styles/custom.css.
			favicon: '/favicon.svg',
			customCss: ['./src/styles/custom.css'],
			// No GitHub social link or "Edit this page" — the repo is private,
			// so both would 404 for every public reader. Do not add them back
			// without first making the repo public, which requires the owner's
			// explicit double confirmation (see CLAUDE.md).
			sidebar: [
				{
					label: 'Getting Started',
					items: [
						{ label: 'Install Petal', slug: 'getting-started/install' },
						{
							label: 'Permissions setup',
							slug: 'getting-started/permissions-setup',
						},
						{
							label: 'Your first meeting',
							slug: 'getting-started/first-meeting',
						},
					],
				},
				{
					label: 'Using Petal',
					items: [
						{
							label: 'Sharing your windows',
							slug: 'using/sharing-your-windows',
						},
						{
							label: 'Viewing shared windows',
							slug: 'using/viewing-shared-windows',
						},
						{
							label: 'Telepointers and drawing',
							slug: 'using/telepointers-and-drawing',
						},
						{ label: 'Remote control', slug: 'using/remote-control' },
						{ label: 'AI chat', slug: 'using/ai-chat' },
						{
							label: 'Cameras and audio',
							slug: 'using/cameras-and-audio',
						},
						{
							label: 'Joining from a browser',
							slug: 'using/joining-from-a-browser',
						},
						{
							label: 'Settings reference',
							slug: 'using/settings-reference',
						},
						{ label: 'Troubleshooting', slug: 'using/troubleshooting' },
					],
				},
				{
					label: 'Customizing',
					items: [
						{
							label: 'Self-hosting Petal',
							slug: 'customizing/self-hosting',
						},
						{
							label: 'Backend API reference',
							slug: 'customizing/backend-api',
						},
						{
							label: 'Invite links and the petal:// scheme',
							slug: 'customizing/invite-links',
						},
					],
				},
			],
			plugins: [
				starlightLinksValidator({
					// Only internal links are checkable offline/in CI; external links
					// (e.g. the LiveKit self-hosting guide linked from self-hosting.md)
					// are left to a human to spot-check.
					errorOnFallbackPages: true,
					errorOnInconsistentLocale: true,
				}),
			],
		}),
	],
});
