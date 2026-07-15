export type VisualBounds = {
	x: number;
	y: number;
	width: number;
	height: number;
};

export type BoundsLabelPlacement =
	| "top-left-outside"
	| "top-right-outside"
	| "bottom-left-outside"
	| "top-left-inside";

type Size = { width: number; height: number };

export function resolveVisualBounds(
	interacting: boolean,
	preview: VisualBounds | undefined,
	selection: VisualBounds,
): VisualBounds {
	return interacting ? selection : (preview ?? selection);
}

export function calculateBoundsLabelPosition(
	bounds: VisualBounds,
	label: Size,
	viewport: Size,
	gap = 8,
): { x: number; y: number; placement: BoundsLabelPlacement } {
	const viewportMargin = 8;
	const maxX = Math.max(
		viewportMargin,
		viewport.width - label.width - viewportMargin,
	);
	const maxY = Math.max(
		viewportMargin,
		viewport.height - label.height - viewportMargin,
	);
	const clampX = (x: number) => Math.max(viewportMargin, Math.min(maxX, x));
	const clampY = (y: number) => Math.max(viewportMargin, Math.min(maxY, y));

	const aboveY = bounds.y - label.height - gap;
	if (aboveY >= viewportMargin) {
		return {
			x: clampX(bounds.x),
			y: aboveY,
			placement: "top-left-outside",
		};
	}

	const rightX = bounds.x + bounds.width + gap;
	if (rightX + label.width <= viewport.width - viewportMargin) {
		return {
			x: rightX,
			y: clampY(bounds.y),
			placement: "top-right-outside",
		};
	}

	const belowY = bounds.y + bounds.height + gap;
	if (belowY + label.height <= viewport.height - viewportMargin) {
		return {
			x: clampX(bounds.x),
			y: belowY,
			placement: "bottom-left-outside",
		};
	}

	return {
		x: clampX(bounds.x + gap),
		y: clampY(bounds.y + gap),
		placement: "top-left-inside",
	};
}
