import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import App from '../App';

// Stub wallet adapters so the hook doesn't hit real browser APIs.
vi.mock('../wallets/freighter', () => ({
  freighterIsAvailable: vi.fn().mockResolvedValue(false),
  freighterGetPublicKey: vi.fn(),
  freighterSign: vi.fn(),
}));
vi.mock('../wallets/albedo', () => ({
  albedoIsAvailable: vi.fn().mockReturnValue(false),
  albedoGetPublicKey: vi.fn(),
  albedoSign: vi.fn(),
}));

describe('App — landing page', () => {
  it('renders the ThemeToggle button on the landing page', () => {
    render(<App />);
    // ThemeToggle renders a button with an aria-label about switching modes.
    const toggleButton = screen.getByRole('button', {
      name: /switch to (light|dark) mode/i,
    });
    expect(toggleButton).toBeDefined();
  });
});
