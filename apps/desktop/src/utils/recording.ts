import { emit } from "@tauri-apps/api/event";
import * as dialog from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import type { MessageKey, MessageParams } from "~/i18n";
import type { createOptionsQuery } from "./queries";
import { commands, type RecordingAction, type RecordingMode } from "./tauri";

export function handleRecordingResult(
	result: Promise<RecordingAction>,
	setOptions: ReturnType<typeof createOptionsQuery>["setOptions"] | undefined,
	t: (key: MessageKey, params?: MessageParams) => string,
) {
	return result
		.then(async (result) => {
			if (result === "Started") return;
			if (result === "InvalidAuthentication") {
				const buttons = setOptions
					? {
							yes: t("recording.login"),
							no: t("recording.switchToStudio"),
							cancel: t("common.cancel"),
						}
					: {
							ok: t("recording.login"),
							cancel: t("common.cancel"),
						};

				const result = await dialog.message(
					t("recording.loginRequiredMessage"),
					{
						title: t("recording.authenticationRequired"),
						buttons,
					},
				);

				if (result === buttons.yes || result === buttons.ok)
					emit("start-sign-in");
				else if (result === buttons.no && setOptions) {
					setOptions({ mode: "studio" });
					commands.setRecordingMode("studio");
				}
			} else if (result === "UpgradeRequired") commands.showWindow("Upgrade");
			else
				await dialog.message(t("recording.startError", { message: result }), {
					title: t("recording.errorStarting"),
				});
		})
		.catch((err) =>
			dialog.message(err, {
				title: t("recording.errorStarting"),
				kind: "error",
			}),
		);
}

export async function openRecordingFolder(
	projectPath: string,
	mode: RecordingMode,
) {
	const path = projectPath.replace(/[/\\]+$/, "");

	const openedContent =
		mode === "instant" &&
		(await commands.openFilePath(`${path}/content`).then(
			() => true,
			() => false,
		));

	if (openedContent) return;

	await revealItemInDir(`${path}/`);
}
