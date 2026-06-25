import { useEffect, useState, useCallback } from 'react';
import {
  all_goose_modes,
  legacy_goose_modes,
  ModeSelectionItem,
  permission_profile_modes,
} from './ModeSelectionItem';
import { useConfig } from '../../ConfigContext';
import { ConversationLimitsDropdown } from './ConversationLimitsDropdown';
import { updateSession } from '../../../api';
import type { GooseMode } from '../../../api/types.gen';
import { defineMessages, useIntl } from '../../../i18n';

const defaultMode: GooseMode = 'auto';

const i18n = defineMessages({
  permissionProfiles: {
    id: 'modeSection.permissionProfiles',
    defaultMessage: 'Permission profiles',
  },
  permissionProfilesDescription: {
    id: 'modeSection.permissionProfilesDescription',
    defaultMessage: 'Choose how Goose handles reads, writes, execution, and approval prompts.',
  },
  compatibilityModes: {
    id: 'modeSection.compatibilityModes',
    defaultMessage: 'Compatibility modes',
  },
  compatibilityModesDescription: {
    id: 'modeSection.compatibilityModesDescription',
    defaultMessage: 'Older mode names remain available for existing workflows.',
  },
});

function isGooseMode(value: unknown): value is GooseMode {
  return all_goose_modes.some((mode) => mode.key === value);
}

export const ModeSection = ({ sessionId }: { sessionId?: string }) => {
  const intl = useIntl();
  const [currentMode, setCurrentMode] = useState<GooseMode>(defaultMode);
  const [maxTurns, setMaxTurns] = useState<number>(1000);
  const { config, read, upsert } = useConfig();

  const handleModeChange = async (newMode: GooseMode) => {
    try {
      if (sessionId) {
        await updateSession({ body: { session_id: sessionId, goose_mode: newMode } });
      }
      await upsert('GOOSE_MODE', newMode, false);
      setCurrentMode(newMode);
    } catch (error) {
      console.error('Error updating goose mode:', error);
      throw new Error(`Failed to store new goose mode: ${newMode}`);
    }
  };

  useEffect(() => {
    const mode = config.GOOSE_MODE;
    if (isGooseMode(mode)) {
      setCurrentMode(mode);
    }
  }, [config.GOOSE_MODE]);

  const fetchMaxTurns = useCallback(async () => {
    try {
      const turns = (await read('GOOSE_MAX_TURNS', false)) as number;
      if (turns) {
        setMaxTurns(turns);
      }
    } catch (error) {
      console.error('Error fetching max turns:', error);
    }
  }, [read]);

  const handleMaxTurnsChange = async (value: number) => {
    try {
      await upsert('GOOSE_MAX_TURNS', value, false);
      setMaxTurns(value);
    } catch (error) {
      console.error('Error updating max turns:', error);
    }
  };

  useEffect(() => {
    fetchMaxTurns();
  }, [fetchMaxTurns]);

  return (
    <div className="space-y-5">
      <section className="space-y-1">
        <div className="px-2 pb-2">
          <h3 className="text-sm font-medium text-text-primary">
            {intl.formatMessage(i18n.permissionProfiles)}
          </h3>
          <p className="text-xs text-text-secondary mt-1">
            {intl.formatMessage(i18n.permissionProfilesDescription)}
          </p>
        </div>
        {permission_profile_modes.map((mode) => (
          <ModeSelectionItem
            key={mode.key}
            mode={mode}
            currentMode={currentMode}
            showDescription={true}
            isApproveModeConfigure={false}
            handleModeChange={handleModeChange}
          />
        ))}
      </section>

      <section className="space-y-1">
        <div className="px-2 pb-2">
          <h3 className="text-sm font-medium text-text-primary">
            {intl.formatMessage(i18n.compatibilityModes)}
          </h3>
          <p className="text-xs text-text-secondary mt-1">
            {intl.formatMessage(i18n.compatibilityModesDescription)}
          </p>
        </div>
        {legacy_goose_modes.map((mode) => (
          <ModeSelectionItem
            key={mode.key}
            mode={mode}
            currentMode={currentMode}
            showDescription={true}
            isApproveModeConfigure={false}
            handleModeChange={handleModeChange}
          />
        ))}
      </section>

      {/* Conversation Limits Dropdown */}
      <ConversationLimitsDropdown maxTurns={maxTurns} onMaxTurnsChange={handleMaxTurnsChange} />
    </div>
  );
};
