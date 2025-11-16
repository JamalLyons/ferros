/**
 * Feature banner hover effects
 * Adds glow effect on hover for feature cards
 */

export function initFeatureHovers() {
    const featureBanners = document.querySelectorAll('[data-feature]');

    featureBanners.forEach((banner) => {
        const glowId = `feature-glow-${banner.dataset.feature}`;
        const glow = document.getElementById(glowId);

        if (glow) {
            banner.addEventListener('mouseenter', () => {
                gsap.to(glow, {
                    opacity: 1,
                    duration: 0.2,
                    ease: 'power2.out'
                });
            });

            banner.addEventListener('mouseleave', () => {
                gsap.to(glow, {
                    opacity: 0,
                    duration: 0.2,
                    ease: 'power2.out'
                });
            });
        }
    });
}

