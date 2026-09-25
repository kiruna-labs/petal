/**
 * Shared motion helpers for Svelte transitions (`svelte/transition`,
 * `svelte/animate`), which take plain-number millisecond durations rather
 * than CSS custom properties -- so they can't pick up tokens.css's existing
 * `@media (prefers-reduced-motion: reduce) { --motion-fast: 0ms; ... }`
 * override automatically the way a CSS `transition: ... var(--motion-base)`
 * does. This module is the one place that bridges the two: components using
 * a JS-driven Svelte transition import a duration from here instead of each
 * re-implementing its own `matchMedia` check (which would risk drifting out
 * of sync with tokens.css's actual ms values).
 *
 * Values mirror tokens.css's semantic motion roles literally -- if those
 * change, update here too. CSS custom properties cannot be passed directly
 * to Svelte's numeric transition APIs, so all JS-driven motion comes through
 * these helpers.
 */

import type { TransitionConfig } from 'svelte/transition';
import type { AnimationConfig } from 'svelte/animate';
import { cubicOut } from 'svelte/easing';
import {
	uniformFlip,
	uniformFlipClipPath,
	uniformFlipTransform,
	uniformFlipVisibleRect,
	type UniformFlip
} from '@petal/shared/logic/tileFlip';

const MOTION_FEEDBACK_MS = 120;
const MOTION_EXIT_MS = 120;
const MOTION_ENTER_MS = 180;
const MOTION_LAYOUT_MS = 220;
const MOTION_DISTANCE_PX = 4;

export function prefersReducedMotion(): boolean {
	if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
	return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/** Duration for quick, incidental motion (hover/press feedback timing). */
export function feedbackDuration(): number {
	return prefersReducedMotion() ? 0 : MOTION_FEEDBACK_MS;
}

/** Duration for a transient surface or state leaving the UI. */
export function exitDuration(): number {
	return prefersReducedMotion() ? 0 : MOTION_EXIT_MS;
}

/** Duration for a transient surface or state entering/replacing content. */
export function enterDuration(): number {
	return prefersReducedMotion() ? 0 : MOTION_ENTER_MS;
}

/** Duration for direct manipulation and transform-based layout reflow. */
export function layoutDuration(): number {
	return prefersReducedMotion() ? 0 : MOTION_LAYOUT_MS;
}

/** Compatibility alias used by existing quick feedback CSS contracts. */
export function fastDuration(): number {
	return feedbackDuration();
}

/** Compatibility alias used by existing state-change transitions. */
export function baseDuration(): number {
	return enterDuration();
}

/** Participant insertion/removal uses the enter/exit role. */
export function tileTransitionDuration(): number {
	return enterDuration();
}

/** Participant FLIP/reflow uses the longer direct-layout role. */
export function tileLayoutDuration(): number {
	return layoutDuration();
}

/**
 * Standard toast enter/exit transition (fade + slight rise + blur), shared by
 * the root ToastHost and the meeting route's local toasts so every toast in
 * the app animates identically. Includes `translateX(-50%)` because the toast
 * anchor is centered via that CSS transform, and a Svelte `transition:`'s
 * inline transform overrides the stylesheet one for the duration of the
 * animation -- dropping it would make the toast jump horizontally while
 * animating.
 */
function surfaceTransition(duration: number): TransitionConfig {
	const distance = prefersReducedMotion() ? 0 : MOTION_DISTANCE_PX;
	return {
		duration,
		css: (t, u) => `
			opacity: ${t};
			transform: translateY(${u * distance}px);
		`
	};
}

export function restrainedSurfaceEnterTransition(_node: Element): TransitionConfig {
	return surfaceTransition(enterDuration());
}

export function restrainedSurfaceExitTransition(_node: Element): TransitionConfig {
	return surfaceTransition(exitDuration());
}

/** Compatibility helper for callers that need one transition directive. */
export function restrainedSurfaceTransition(_node: Element): TransitionConfig {
	return surfaceTransition(enterDuration());
}

export function toastTransition(_node: Element): TransitionConfig {
	const distance = prefersReducedMotion() ? 0 : MOTION_DISTANCE_PX * 2;
	return {
		duration: enterDuration(),
		css: (t, u) => `
			opacity: ${t};
			transform: translateX(-50%) translateY(${u * distance}px);
			filter: blur(${u * 2}px);
		`
	};
}

/** Each node's running keyed-list FLIP: its frame, how far back from rest
 * it last painted (`remaining`, 1 -> 0), and when it ends (performance.now()
 * ms). */
interface TileFlipInFlight {
	flip: UniformFlip;
	remaining: number;
	end: number;
}

const uniformTileFlips = new WeakMap<Element, TileFlipInFlight>();

/**
 * True while `uniformTileFlip` is animating `node`. Svelte owns those
 * animations (no WAAPI handle to ask), so a layout pass that interrupts one
 * asks here whether the tile is clipped and must be retargeted from its
 * visible box (`visibleFlipRect`) rather than its larger painted one.
 */
export function uniformTileFlipInFlight(node: Element, now = performance.now()): boolean {
	const flight = uniformTileFlips.get(node);
	return flight !== undefined && flight.remaining > 0 && now < flight.end;
}

/**
 * Keyed-list FLIP for gallery tiles (#248): a drop-in for `svelte/animate`'s
 * `flip`, whose `scale(sx, sy)` squashes live video whenever a join or leave
 * changes the tile's shape (camera tiles crop, so they now do). Uses the
 * shared `uniformFlip` frame -- one uniform scale plus a clip of the box --
 * that web-harness/src/tileReflow.ts uses too.
 */
export function uniformTileFlip(
	node: Element,
	{ from, to }: { from: DOMRect; to: DOMRect },
	params: { duration?: number; easing?: (t: number) => number } = {}
): AnimationConfig {
	// Interrupting another keyed-list FLIP: Svelte measured `from` with
	// getBoundingClientRect() -- that FLIP's transform but not its clip -- and
	// has cancelled it before calling here, so start from the box its last
	// frame actually showed, or the tile pops out to its unclipped size.
	const interrupted = uniformTileFlipInFlight(node) ? uniformTileFlips.get(node) : undefined;
	uniformTileFlips.delete(node);
	const flip = uniformFlip(
		interrupted ? uniformFlipVisibleRect(from, interrupted.flip, interrupted.remaining) : from,
		to
	);
	if (!flip) return { duration: 0 };
	const radius =
		typeof getComputedStyle === 'function' ? parseFloat(getComputedStyle(node).borderTopLeftRadius) || 0 : 0;
	const duration = params.duration ?? layoutDuration();
	const flight: TileFlipInFlight = { flip, remaining: 1, end: performance.now() + duration };
	if (duration > 0) uniformTileFlips.set(node, flight);
	return {
		duration,
		easing: params.easing ?? cubicOut,
		// Svelte calls this every frame with the animation's own progress (it
		// starts a frame or two after this call), so an interruption knows the
		// exact frame the tile was showing.
		tick: (_t, u) => {
			flight.remaining = u;
		},
		css: (_t, u) => {
			const clipPath = uniformFlipClipPath(flip, u, radius);
			return `transform: ${uniformFlipTransform(flip, u)}; transform-origin: top left;${clipPath ? ` clip-path: ${clipPath};` : ''}`;
		}
	};
}
