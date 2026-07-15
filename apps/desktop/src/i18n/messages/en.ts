export const enMessages = {
	"common.cancel": "Cancel",
	"common.close": "Close",
	"common.confirm": "Confirm",
	"common.delete": "Delete",
	"common.error": "Error",
	"common.loading": "Loading…",
	"common.retry": "Retry",
	"common.save": "Save",
	"language.english": "English",
	"language.interface": "Interface language",
	"language.simplifiedChinese": "Simplified Chinese",
	"language.system": "System default",
} as const;

export type MessageKey = keyof typeof enMessages;
