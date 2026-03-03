<?php

namespace App;

/**
 * Base handler with common response helpers.
 */
class BaseHandler
{
    protected TimerService $timer;

    public function __construct()
    {
        $this->timer = new TimerService();
    }

    /**
     * Return a successful response.
     *
     * @param mixed $data
     * @return array
     */
    protected function okResponse($data): array
    {
        return ['status' => 'ok', 'data' => $data];
    }
}
