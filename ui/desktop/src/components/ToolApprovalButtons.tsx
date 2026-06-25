import { useState, useEffect } from 'react';
import { Button } from './ui/button';
import { confirmToolAction, Permission } from '../api';
import { defineMessages, useIntl } from '../i18n';

const i18n = defineMessages({
  allowOnce: {
    id: 'toolApprovalButtons.allowOnce',
    defaultMessage: 'Allow Once',
  },
  alwaysAllow: {
    id: 'toolApprovalButtons.alwaysAllow',
    defaultMessage: 'Always Allow',
  },
  deny: {
    id: 'toolApprovalButtons.deny',
    defaultMessage: 'Deny',
  },
  allowedOnce: {
    id: 'toolApprovalButtons.allowedOnce',
    defaultMessage: 'Allowed once',
  },
  alwaysAllowed: {
    id: 'toolApprovalButtons.alwaysAllowed',
    defaultMessage: 'Always allowed',
  },
  denied: {
    id: 'toolApprovalButtons.denied',
    defaultMessage: 'Denied',
  },
  deniedOnce: {
    id: 'toolApprovalButtons.deniedOnce',
    defaultMessage: 'Denied once',
  },
  cancelled: {
    id: 'toolApprovalButtons.cancelled',
    defaultMessage: 'Cancelled',
  },
  approving: {
    id: 'toolApprovalButtons.approving',
    defaultMessage: 'Approving...',
  },
  approvalFailed: {
    id: 'toolApprovalButtons.approvalFailed',
    defaultMessage:
      'Approval could not be delivered. The request may have expired. Try rerunning the action.',
  },
});

const globalApprovalState = new Map<
  string,
  {
    decision: Permission | null;
    isClicked: boolean;
    errorMessage: string | null;
  }
>();

export interface ToolApprovalData {
  id: string;
  toolName: string;
  prompt?: string;
  sessionId: string;
  isClicked?: boolean;
}

export default function ToolApprovalButtons({ data }: { data: ToolApprovalData }) {
  const intl = useIntl();
  const { id, toolName, prompt, sessionId, isClicked: initialIsClicked } = data;

  const storedState = globalApprovalState.get(id);
  const [decision, setDecision] = useState<Permission | null>(storedState?.decision ?? null);
  const [isClicked, setIsClicked] = useState(storedState?.isClicked ?? initialIsClicked ?? false);
  const [pendingAction, setPendingAction] = useState<Permission | null>(null);
  const [errorMessage, setErrorMessage] = useState<string | null>(
    storedState?.errorMessage ?? null
  );

  useEffect(() => {
    const currentState = globalApprovalState.get(id);
    if (currentState) {
      setDecision(currentState.decision);
      setIsClicked(currentState.isClicked);
      setErrorMessage(currentState.errorMessage);
      setPendingAction(null);
    }
  }, [id]);

  useEffect(() => {
    globalApprovalState.set(id, { decision, isClicked, errorMessage });
  }, [id, decision, isClicked, errorMessage]);

  const approvalErrorText = (error: unknown): string => {
    if (error && typeof error === 'object' && 'message' in error) {
      const message = (error as { message?: unknown }).message;
      if (typeof message === 'string' && message.trim()) {
        return message;
      }
    }
    return intl.formatMessage(i18n.approvalFailed);
  };

  const handleAction = async (action: Permission) => {
    setPendingAction(action);
    setErrorMessage(null);

    try {
      const response = await confirmToolAction({
        body: {
          sessionId,
          id,
          action,
          principalType: 'Tool',
        },
      });
      if (response.error) {
        throw response.error;
      }
      setDecision(action);
      setIsClicked(true);
    } catch (err) {
      console.error('Error confirming tool action:', err);
      setDecision(null);
      setIsClicked(false);
      setErrorMessage(approvalErrorText(err));
    } finally {
      setPendingAction(null);
    }
  };

  if (isClicked && decision) {
    const statusMessages: Record<Permission, string> = {
      allow_once: intl.formatMessage(i18n.allowedOnce),
      always_allow: intl.formatMessage(i18n.alwaysAllowed),
      always_deny: intl.formatMessage(i18n.denied),
      deny_once: intl.formatMessage(i18n.deniedOnce),
      cancel: intl.formatMessage(i18n.cancelled),
    };
    return (
      <p className="text-sm text-muted-foreground mt-2">
        {toolName} - {statusMessages[decision]}
      </p>
    );
  }

  return (
    <div className="mt-2">
      <div className="flex items-center gap-2">
        <Button
          className="rounded-full"
          variant="secondary"
          disabled={pendingAction !== null}
          onClick={() => handleAction('allow_once')}
        >
          {pendingAction === 'allow_once'
            ? intl.formatMessage(i18n.approving)
            : intl.formatMessage(i18n.allowOnce)}
        </Button>
        {!prompt && (
          <Button
            className="rounded-full"
            variant="secondary"
            disabled={pendingAction !== null}
            onClick={() => handleAction('always_allow')}
          >
            {pendingAction === 'always_allow'
              ? intl.formatMessage(i18n.approving)
              : intl.formatMessage(i18n.alwaysAllow)}
          </Button>
        )}
        <Button
          className="rounded-full"
          variant="outline"
          disabled={pendingAction !== null}
          onClick={() => handleAction('deny_once')}
        >
          {pendingAction === 'deny_once'
            ? intl.formatMessage(i18n.approving)
            : intl.formatMessage(i18n.deny)}
        </Button>
      </div>
      {errorMessage && (
        <p role="alert" className="mt-2 text-sm text-text-danger">
          {errorMessage}
        </p>
      )}
    </div>
  );
}
