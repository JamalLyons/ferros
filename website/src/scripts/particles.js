/**
 * Floating particles animation in hero section
 * Optimized for mobile devices
 */

export function initParticles() {
    const particlesContainer = document.getElementById('particles');
    if (!particlesContainer) return;

    const isMobile = window.innerWidth < 768;
    const particleCount = isMobile ? 15 : 30;

    for (let i = 0; i < particleCount; i++) {
        const particle = document.createElement('div');
        particle.className = 'particle';

        particle.style.left = `${Math.random() * 100}%`;
        particle.style.top = `${Math.random() * 100}%`;

        const delay = Math.random() * 8;
        const duration = 8 + Math.random() * 4;
        particle.style.animationDelay = `${delay}s`;
        particle.style.animationDuration = `${duration}s`;

        particlesContainer.appendChild(particle);
    }
}

