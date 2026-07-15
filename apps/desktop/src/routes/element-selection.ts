import type { CropBounds } from "~/components/Cropper";

type LogicalBounds = {
	position: { x: number; y: number };
	size: { width: number; height: number };
};

type MinimumSize = { width: number; height: number };

function toCropBounds(bounds: LogicalBounds): CropBounds {
	return {
		x: bounds.position.x,
		y: bounds.position.y,
		width: bounds.size.width,
		height: bounds.size.height,
	};
}

function isValidCandidate(bounds: CropBounds, minimum: MinimumSize) {
	return (
		Number.isFinite(bounds.x) &&
		Number.isFinite(bounds.y) &&
		Number.isFinite(bounds.width) &&
		Number.isFinite(bounds.height) &&
		bounds.width >= minimum.width &&
		bounds.height >= minimum.height
	);
}

export function buildElementCandidates(
	elementBounds: LogicalBounds[],
	windowBounds: LogicalBounds | undefined,
	minimum: MinimumSize,
): CropBounds[] {
	const candidates = windowBounds
		? [...elementBounds, windowBounds]
		: elementBounds;
	const result: CropBounds[] = [];

	for (const candidate of candidates) {
		const bounds = toCropBounds(candidate);
		if (!isValidCandidate(bounds, minimum)) continue;
		if (
			result.some(
				(existing) =>
					existing.x === bounds.x &&
					existing.y === bounds.y &&
					existing.width === bounds.width &&
					existing.height === bounds.height,
			)
		)
			continue;
		result.push(bounds);
	}

	return result;
}

export function cycleElementLevel(
	current: number,
	deltaY: number,
	candidateCount: number,
) {
	if (candidateCount === 0 || deltaY === 0) return current;
	const next = current + (deltaY < 0 ? 1 : -1);
	return Math.max(0, Math.min(candidateCount - 1, next));
}

export function hasAreaSelection(bounds: CropBounds) {
	return bounds.width > 1 && bounds.height > 1;
}

export function areaContextAction(bounds: CropBounds) {
	return hasAreaSelection(bounds) ? "clear" : "exit";
}

export function elementCandidateSignature(candidates: CropBounds[]) {
	return candidates
		.map((bounds) => `${bounds.x},${bounds.y},${bounds.width},${bounds.height}`)
		.join(";");
}
