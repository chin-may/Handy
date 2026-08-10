import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown, type DropdownOption } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import type { RecordingMode } from "@/bindings";

interface RecordingModeProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const RecordingModeSetting: React.FC<RecordingModeProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const recordingMode =
      (getSetting("recording_mode") as RecordingMode | undefined) ??
      "tap_or_hold";
    const options: DropdownOption[] = [
      {
        value: "tap_or_hold",
        label: t("settings.general.recordingMode.options.tapOrHold"),
      },
      {
        value: "push_to_talk",
        label: t("settings.general.recordingMode.options.pushToTalk"),
      },
      {
        value: "toggle",
        label: t("settings.general.recordingMode.options.toggle"),
      },
    ];

    return (
      <SettingContainer
        title={t("settings.general.recordingMode.label")}
        description={t("settings.general.recordingMode.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={options}
          selectedValue={recordingMode}
          onSelect={(value) =>
            updateSetting("recording_mode", value as RecordingMode)
          }
          disabled={isUpdating("recording_mode")}
        />
      </SettingContainer>
    );
  },
);
