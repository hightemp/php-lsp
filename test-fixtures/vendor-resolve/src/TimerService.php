<?php

namespace App;

/**
 * Timer service for tracking elapsed time.
 */
class TimerService
{
    /**
     * Start a named timer.
     *
     * @param string $name
     * @return void
     */
    public function start(string $name): void
    {
    }

    /**
     * Stop a named timer.
     *
     * @param string $name
     * @return float
     */
    public function stop(string $name): float
    {
        return 0.0;
    }
}
