import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, type RenderOptions, screen, waitFor } from '@testing-library/react';
import ToolCallConfirmation from './ToolCallConfirmation';
import { IntlTestWrapper } from '../i18n/test-utils';
import type { ActionRequired } from '../api';
import { confirmToolAction } from '../api';

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return {
    ...actual,
    confirmToolAction: vi.fn(),
  };
});

const mockConfirmToolAction = vi.mocked(confirmToolAction);

const renderWithIntl = (ui: React.ReactElement, options?: RenderOptions) =>
  render(ui, { wrapper: IntlTestWrapper, ...options });

describe('ToolCallConfirmation', () => {
  beforeEach(() => {
    mockConfirmToolAction.mockReset();
  });

  it('shows the subagent approval prompt body', () => {
    const actionRequiredContent = {
      type: 'actionRequired',
      data: {
        actionType: 'toolConfirmation',
        id: 'subagent:session_1:req_1',
        toolName: 'developer__shell',
        arguments: { command: 'touch x' },
        prompt: 'Subagent `session_1` requests approval:\n\nconfirm shell',
      },
    } satisfies ActionRequired & { type: 'actionRequired' };

    renderWithIntl(
      <ToolCallConfirmation
        sessionId="parent_session"
        isClicked={false}
        actionRequiredContent={actionRequiredContent}
      />
    );

    expect(screen.getByText('Allow Shell?')).toBeInTheDocument();
    expect(screen.getByText(/Subagent `session_1` requests approval/)).toBeInTheDocument();
    expect(screen.getByText(/confirm shell/)).toBeInTheDocument();
  });

  it('does not show success when approval delivery fails', async () => {
    mockConfirmToolAction.mockResolvedValue({
      data: undefined,
      error: { message: 'Tool confirmation request was not found or has expired' },
      request: new Request('http://localhost/action-required/tool-confirmation'),
      response: new Response(null, { status: 404 }),
    } as never);

    const actionRequiredContent = {
      type: 'actionRequired',
      data: {
        actionType: 'toolConfirmation',
        id: 'expired_req',
        toolName: 'developer__shell',
        arguments: { command: 'touch x' },
      },
    } satisfies ActionRequired & { type: 'actionRequired' };

    renderWithIntl(
      <ToolCallConfirmation
        sessionId="parent_session"
        isClicked={false}
        actionRequiredContent={actionRequiredContent}
      />
    );

    fireEvent.click(screen.getByRole('button', { name: 'Allow Once' }));

    await screen.findByRole('alert');
    expect(
      screen.getByText('Tool confirmation request was not found or has expired')
    ).toBeInTheDocument();
    expect(screen.queryByText(/Allowed once/)).not.toBeInTheDocument();

    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Allow Once' })).toBeEnabled();
    });
  });
});
